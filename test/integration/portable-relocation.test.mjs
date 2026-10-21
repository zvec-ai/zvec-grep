import assert from "node:assert/strict";
import {
  cp,
  mkdir,
  readFile,
  rename,
  rm,
  stat,
  symlink,
  utimes,
  writeFile,
} from "node:fs/promises";
import { dirname, join } from "node:path";
import test from "node:test";
import { readWorkspaceManifest } from "../../dist/engine/manifest.js";
import { createZvecGrep } from "../../dist/index.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { restoreIndexedMtime } from "../helpers/mtime.mjs";
import { CountingEmbeddingModel } from "../helpers/counting-embedding.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";
import { endsWithRelative, physicallyUnder } from "../helpers/native-path.mjs";

useIsolatedZvecGrepHome();

const FIXTURES = {
  "docs/guide.md": [
    "# Getting started",
    "",
    "Install the tool and run the indexer on the workspace.",
    "",
    "## Configuration",
    "",
    "Set the model and the device in the configuration file.",
    "",
  ].join("\n"),
  "src/util.ts": [
    "export function normalizeEndpoint(value: string): string {",
    "  return value.trim().toLowerCase();",
    "}",
    "",
  ].join("\n"),
  "notes/plain.txt": "Relocation probe note about portable indexes.\n",
};

async function writeFixtures(root, files = FIXTURES) {
  for (const [relative, content] of Object.entries(files)) {
    const target = join(root, relative);
    await mkdir(dirname(target), { recursive: true });
    await writeFile(target, content);
  }
}

function hitPaths(result) {
  return result.items.map((item) => item.file?.absolutePath ?? "");
}

test("copied workspace resolves destinations at the new location", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-portable-");
  const A = join(parent, "A");
  const B = join(parent, "B");
  await mkdir(A, { recursive: true });
  await writeFixtures(A);

  const modelA = new CountingEmbeddingModel();
  const serviceA = await createZvecGrep({ root: A, embeddingModel: modelA });
  const indexed = await serviceA.index();
  assert.ok(indexed.filesAdded > 0);
  await serviceA.close();

  await cp(A, B, { recursive: true });

  // B searches its own copy while A still exists: every destination is B's.
  const modelB = new CountingEmbeddingModel();
  const serviceB = await createZvecGrep({ root: B, embeddingModel: modelB });
  const whileAExists = await serviceB.context({
    query: "configuration device",
    limit: 5,
  });
  assert.ok(whileAExists.items.length > 0);
  for (const file of hitPaths(whileAExists)) {
    assert.ok(
      physicallyUnder(file, B),
      `destination must resolve under B: ${file}`,
    );
  }
  assert.equal(modelB.counts.document, 0);

  // With A unavailable, B keeps working from its own rebound copy.
  await rename(A, `${A}-away`);
  const afterAGone = await serviceB.context({
    query: "configuration device",
    limit: 5,
  });
  assert.ok(afterAGone.items.length > 0);
  for (const file of hitPaths(afterAGone)) {
    assert.ok(physicallyUnder(file, B), `destination under B: ${file}`);
  }
  await rename(`${A}-away`, A);
  await serviceB.close();
});

test("refresh after relocation reuses vectors with zero document embeddings", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-portable-refresh-");
  const A = join(parent, "A");
  const B = join(parent, "B");
  await mkdir(A, { recursive: true });
  await writeFixtures(A);

  const serviceA = await createZvecGrep({
    root: A,
    embeddingModel: new CountingEmbeddingModel(),
  });
  await serviceA.index();
  await serviceA.close();

  await cp(A, B, { recursive: true });

  // Change one file at B; the auto-update reconciles the relocated copy.
  await writeFile(
    join(B, "notes", "plain.txt"),
    "Relocation probe note about portable indexes, extended.\n",
  );
  const modelB = new CountingEmbeddingModel();
  const serviceB = await createZvecGrep({ root: B, embeddingModel: modelB });
  const result = await serviceB.context({
    query: "portable indexes",
    limit: 5,
  });
  assert.ok(result.items.length > 0);
  // Only the changed file may be re-embedded; everything else reuses vectors.
  const status = await serviceB.info();
  assert.equal(status.status?.filesModified ?? 0, 0);
  assert.ok(
    modelB.counts.document <= 1,
    `expected at most 1 document embedding, got ${modelB.counts.document}`,
  );
  for (const file of hitPaths(result)) {
    assert.ok(physicallyUnder(file, B), `destination under B: ${file}`);
  }
  await serviceB.close();
});

test("coexisting copies are isolated: operations on B never touch A", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-portable-isolation-");
  const A = join(parent, "A");
  const B = join(parent, "B");
  await mkdir(A, { recursive: true });
  await writeFixtures(A);

  const serviceA = await createZvecGrep({
    root: A,
    embeddingModel: new CountingEmbeddingModel(),
  });
  await serviceA.index();
  await serviceA.close();

  await cp(A, B, { recursive: true });
  await writeFile(
    join(B, "notes", "plain.txt"),
    "B-only content about a completely different subject.\n",
  );

  const aHomeStat = await stat(join(A, ".zvec-grep", "manifest.json"));

  const serviceB = await createZvecGrep({
    root: B,
    embeddingModel: new CountingEmbeddingModel(),
  });
  await serviceB.context({ query: "different subject", limit: 5 });
  const bSearch = await serviceB.context({
    query: "completely different subject",
    limit: 5,
  });
  await serviceB.close();

  const aHomeStatAfter = await stat(join(A, ".zvec-grep", "manifest.json"));
  assert.equal(
    aHomeStatAfter.mtimeMs,
    aHomeStat.mtimeMs,
    "A's manifest must not be modified by operations on B",
  );
  for (const file of hitPaths(bSearch)) {
    assert.ok(!physicallyUnder(file, A), `B must not read A: ${file}`);
  }

  // A still serves its own original content.
  const serviceA2 = await createZvecGrep({
    root: A,
    embeddingModel: new CountingEmbeddingModel(),
  });
  const aSearch = await serviceA2.context({
    query: "relocation probe note",
    limit: 5,
  });
  await serviceA2.close();
  for (const file of hitPaths(aSearch)) {
    assert.ok(physicallyUnder(file, A), `A must serve A: ${file}`);
  }
});

test("timestamp-only changes reuse existing vectors", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-portable-mtime-");
  const root = join(parent, "W");
  await mkdir(root, { recursive: true });
  await writeFixtures(root);

  const model = new CountingEmbeddingModel();
  const service = await createZvecGrep({ root, embeddingModel: model });
  await service.index();
  const before = model.counts.document;

  const target = join(root, "notes", "plain.txt");
  const now = new Date();
  await utimes(target, now, now);
  await service.context({ query: "portable indexes", limit: 3 });

  assert.equal(model.counts.document, before);
  await service.close();
});

test("reconciliation detects changed content with unchanged size and mtime", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-portable-samestat-");
  const A = join(parent, "A");
  const B = join(parent, "B");
  await mkdir(A, { recursive: true });
  await writeFixtures(A);

  const serviceA = await createZvecGrep({
    root: A,
    embeddingModel: new CountingEmbeddingModel(),
  });
  await serviceA.index();
  await serviceA.close();

  await cp(A, B, { recursive: true });

  // Same size, same mtime, different content at B. The indexed stat values
  // are captured BEFORE the edit and restored exactly afterwards.
  const target = join(B, "notes", "plain.txt");
  const indexedStat = await stat(target);
  const original = await readFile(target, "utf8");
  const replaced = original.replace("portable", "PORTABLE");
  assert.equal(replaced.length, original.length);
  await writeFile(target, replaced);
  await restoreIndexedMtime(target, indexedStat.mtimeMs);

  const modelB = new CountingEmbeddingModel();
  const serviceB = await createZvecGrep({ root: B, embeddingModel: modelB });
  // Explicit index: the relocated copy has no established binding, so the
  // run reconciles by hashing content instead of trusting stored stat
  // metadata. The same-stat edit must be detected.
  const result = await serviceB.index();
  assert.ok(
    result.filesModified >= 1,
    `expected the same-stat edit to be detected, got ${JSON.stringify({
      added: result.filesAdded,
      modified: result.filesModified,
    })}`,
  );
  const search = await serviceB.context({ query: "PORTABLE", limit: 3 });
  assert.ok(search.items.length > 0);
  assert.ok(
    hitPaths(search).some((file) => endsWithRelative(file, "notes/plain.txt")),
    "the same-stat edit must be searchable after reconciliation",
  );
  await serviceB.close();
});

test("edit, add, delete and rename behave incrementally without orphans", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-portable-incremental-");
  const root = join(parent, "W");
  await mkdir(root, { recursive: true });
  await writeFixtures(root);

  const model = new CountingEmbeddingModel();
  const service = await createZvecGrep({ root, embeddingModel: model });
  await service.index();

  await writeFile(join(root, "docs", "new.md"), "# New\n\nAdded document.\n");
  await writeFile(
    join(root, "notes", "plain.txt"),
    "Edited note about portable indexes.\n",
  );
  await rm(join(root, "src", "util.ts"));
  await rename(
    join(root, "docs", "guide.md"),
    join(root, "docs", "renamed.md"),
  );

  const result = await service.index();
  assert.equal(result.filesAdded, 2); // new.md + renamed.md
  assert.equal(result.filesModified, 1); // plain.txt
  assert.equal(result.filesDeleted, 2); // util.ts + guide.md

  const status = await service.info();
  assert.equal(status.status?.filesPending ?? 1, 0);
  assert.equal(status.status?.filesFailed ?? 1, 0);

  const renamed = await service.context({
    query: "configuration device",
    limit: 5,
  });
  assert.ok(
    hitPaths(renamed).some((file) => endsWithRelative(file, "docs/renamed.md")),
    "renamed file must be searchable",
  );
  const removed = await service.context({
    query: "normalizeEndpoint",
    limit: 5,
  });
  assert.ok(
    !hitPaths(removed).some((file) => endsWithRelative(file, "src/util.ts")),
    "deleted file must not be searchable",
  );
  await service.close();
});

test("two live copies sharing an index UUID work independently", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-portable-copies-");
  const A = join(parent, "A");
  const B = join(parent, "B");
  await mkdir(A, { recursive: true });
  await writeFixtures(A);

  const serviceA = await createZvecGrep({
    root: A,
    embeddingModel: new CountingEmbeddingModel(),
  });
  await serviceA.index();
  await serviceA.close();
  await cp(A, B, { recursive: true });

  const manifestA = readWorkspaceManifest(join(A, ".zvec-grep"));
  const manifestB = readWorkspaceManifest(join(B, ".zvec-grep"));
  assert.equal(manifestA?.id, manifestB?.id);

  const modelA = new CountingEmbeddingModel();
  const modelB = new CountingEmbeddingModel();
  const serviceA2 = await createZvecGrep({ root: A, embeddingModel: modelA });
  const serviceB2 = await createZvecGrep({ root: B, embeddingModel: modelB });

  const [resultA, resultB] = await Promise.all([
    serviceA2.context({ query: "configuration", limit: 3 }),
    serviceB2.context({ query: "configuration", limit: 3 }),
  ]);
  assert.ok(resultA.items.length > 0);
  assert.ok(resultB.items.length > 0);

  // Independent writers: both indexes update their own copies.
  await writeFile(join(A, "notes", "a-only.txt"), "A-only addition.\n");
  await writeFile(join(B, "notes", "b-only.txt"), "B-only addition.\n");
  await serviceA2.index();
  await serviceB2.index();
  assert.ok(modelA.counts.document >= 1);
  assert.ok(modelB.counts.document >= 1);
  await serviceA2.close();
  await serviceB2.close();
});

test("legacy (v1) manifests are rejected with migration guidance", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-portable-legacy-");
  const root = join(parent, "W");
  await mkdir(join(root, ".zvec-grep"), { recursive: true });
  await writeFile(
    join(root, ".zvec-grep", "manifest.json"),
    JSON.stringify({
      manifestVersion: 1,
      id: "legacy-id",
      name: "legacy",
      path: join(root, ".zvec-grep"),
      rootPaths: [{ absolutePath: root, recursive: true }],
      indexPolicy: "enabled",
      embedding: null,
      indexVersion: null,
      createdTime: 1,
      updatedTime: 1,
      embeddingRuntime: {},
    }),
  );

  const service = await createZvecGrep({
    root,
    embeddingModel: new CountingEmbeddingModel(),
  });
  await assert.rejects(
    () => service.context({ query: "anything", limit: 1 }),
    /legacy absolute-path format.*migration|migration/i,
  );
  await service.close();
});

test("indexing never persists credentials or device in the manifest", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-portable-credentials-");
  const root = join(parent, "W");
  await mkdir(root, { recursive: true });
  await writeFixtures(root);

  const model = new CountingEmbeddingModel();
  const service = await createZvecGrep({
    root,
    embeddingModel: model,
    apiKey: "secret-test-key",
    endpoint: "http://127.0.0.1:9/embeddings",
  });
  await service.index();
  await service.close();

  const raw = await readFile(join(root, ".zvec-grep", "manifest.json"), "utf8");
  assert.ok(!raw.includes("secret-test-key"), "manifest must not carry keys");
  const manifest = JSON.parse(raw);
  assert.equal(manifest.embeddingRuntime.apiKey, undefined);
  assert.equal(manifest.embeddingRuntime.device, undefined);
  assert.equal(
    manifest.embeddingRuntime.endpoint,
    "http://127.0.0.1:9/embeddings",
  );
});

test("explicit index roots inherit persisted scoping through workspace aliases", async (t) => {
  const parent = await createTemporaryDirectory(t, "zg-portable-scope-");
  const physical = join(parent, "real", "repo");
  await mkdir(join(physical, "src"), { recursive: true });
  await writeFile(join(physical, "src", "inside.ts"), "export const I = 1;\n");
  await symlink(join(parent, "real"), join(parent, "var"));

  const model = new CountingEmbeddingModel();
  const service = await createZvecGrep({
    root: physical,
    embeddingModel: model,
  });
  await service.index({
    rootPaths: [{ absolutePath: physical, recursive: true, globs: ["src/**"] }],
  });

  // A caller addresses the same workspace through the alias spelling with
  // an unscoped explicit root: the persisted scoping must be inherited,
  // not silently replaced (hosted macOS e2e regression, round 34).
  await writeFile(join(physical, "outside.ts"), "export const O = 2;\n");
  const result = await service.index({
    rootPaths: [{ absolutePath: join(parent, "var", "repo"), recursive: true }],
  });
  assert.equal(
    result.filesAdded,
    0,
    "an out-of-scope file must not be indexed through an alias-spelled root",
  );
  const search = await service.context({
    query: "O = 2",
    route: "fts",
    autoUpdate: false,
  });
  assert.ok(
    hitPaths(search).every((file) => !endsWithRelativeFix(file, "outside.ts")),
    "the out-of-scope file must not be searchable",
  );
  await service.close();
});

function endsWithRelativeFix(file, suffix) {
  return file.split(/[\\/]/).join("/").endsWith(suffix);
}
