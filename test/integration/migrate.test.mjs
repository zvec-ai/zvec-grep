import assert from "node:assert/strict";
import { readdirSync } from "node:fs";
import {
  cp,
  mkdir,
  readFile,
  readdir,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { ZVecInitialize, ZVecLogLevel, ZVecOpen } from "@zvec/zvec";
import { readWorkspaceManifest } from "../../dist/engine/manifest.js";
import { migrateWorkspaceIndex } from "../../dist/engine/migrate/index.js";
import { resolveWorkspaceIndexStoragePaths } from "../../dist/engine/storage/layout.js";
import { createZvecGrep } from "../../dist/index.js";
import { CountingEmbeddingModel } from "../helpers/counting-embedding.mjs";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { restoreIndexedMtime } from "../helpers/mtime.mjs";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";
import { physicallyUnder } from "../helpers/native-path.mjs";
import { buildLegacyHome } from "../helpers/legacy-index.mjs";

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
};

async function makeSourceWorkspace(t, parent) {
  const sourceRoot = join(parent, "original");
  await mkdir(join(sourceRoot, "docs"), { recursive: true });
  await mkdir(join(sourceRoot, "src"), { recursive: true });
  for (const [relative, content] of Object.entries(FIXTURES)) {
    await writeFile(join(sourceRoot, relative), content);
  }
  const service = await createZvecGrep({
    root: sourceRoot,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await service.index();
  await service.close();
  const manifest = readWorkspaceManifest(join(sourceRoot, ".zvec-grep"));
  return { sourceRoot, manifest };
}

async function copySourceFiles(sourceRoot, destinationRoot) {
  await mkdir(destinationRoot, { recursive: true });
  await cp(join(sourceRoot, "docs"), join(destinationRoot, "docs"), {
    recursive: true,
  });
  await cp(join(sourceRoot, "src"), join(destinationRoot, "src"), {
    recursive: true,
  });
}

// Descriptor accounting reads /proc/self/fd. Only its absence (ENOENT —
// the filesystem does not expose descriptors, e.g. Windows) makes this
// Linux-only coverage unavailable; any other error propagates rather than
// silently skipping the no-leak assertion. The close/reopen, cleanup and
// retry assertions below carry the guarantee on every platform.
function fdCount() {
  try {
    return readdirSync("/proc/self/fd").length;
  } catch (error) {
    if (error.code === "ENOENT") {
      return null;
    }
    throw error;
  }
}

test("migration converts a legacy index preserving vectors and relationships", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-migrate-");
  const { sourceRoot, manifest } = await makeSourceWorkspace(t, parent);
  const legacyHome = join(sourceRoot, ".zvec-grep-legacy");
  const { fileCount, entityCount } = await buildLegacyHome(
    sourceRoot,
    legacyHome,
    manifest.id,
  );
  assert.ok(fileCount >= 2);
  assert.ok(entityCount >= 2);

  const destinationRoot = join(parent, "destination");
  await copySourceFiles(sourceRoot, destinationRoot);

  const sourceManifestBefore = await readFile(
    join(legacyHome, "manifest.json"),
    "utf8",
  );
  const result = await migrateWorkspaceIndex({
    sourceHome: legacyHome,
    destinationRoot,
  });

  assert.equal(result.filesConverted, fileCount);
  assert.equal(result.entitiesConverted, entityCount);
  assert.deepEqual(result.missingFiles, []);
  assert.equal(result.droppedPersistedCredential, true);
  assert.equal(result.droppedPersistedDevice, true);
  assert.equal(result.verification.countsMatch, true);
  assert.equal(result.verification.identitiesUnique, true);
  assert.equal(result.verification.ownershipValid, true);
  assert.equal(result.verification.inventoriesExact, true);
  assert.equal(result.verification.groupIntegrity, true);
  assert.equal(result.verification.vectorsExact, true);
  assert.ok(result.verification.vectorsCompared > 0);

  const migratedManifest = readWorkspaceManifest(result.destinationHome);
  assert.equal(migratedManifest?.id, manifest.id);
  const destinationPaths = resolveWorkspaceIndexStoragePaths(
    result.destinationHome,
  );
  const referencePaths = resolveWorkspaceIndexStoragePaths(
    join(sourceRoot, ".zvec-grep"),
  );
  const migratedEntities = ZVecOpen(destinationPaths.indexPath, {
    readOnly: true,
  });
  const referenceEntities = ZVecOpen(referencePaths.indexPath, {
    readOnly: true,
  });
  const migratedIds = [
    ...migratedEntities.iterDocsSync({ includeVector: true }),
  ]
    .map((doc) => doc.id)
    .sort();
  const referenceIds = [
    ...referenceEntities.iterDocsSync({ includeVector: true }),
  ]
    .map((doc) => doc.id)
    .sort();
  assert.deepEqual(migratedIds, referenceIds);
  const referenceById = new Map(
    [...referenceEntities.iterDocsSync({ includeVector: true })].map((doc) => [
      doc.id,
      Array.from(doc.vectors.embedding),
    ]),
  );
  for (const doc of [
    ...migratedEntities.iterDocsSync({ includeVector: true }),
  ]) {
    assert.deepEqual(
      Array.from(doc.vectors.embedding),
      referenceById.get(doc.id),
    );
  }
  migratedEntities.closeSync();
  referenceEntities.closeSync();

  const migratedRaw = await readFile(
    join(result.destinationHome, "manifest.json"),
    "utf8",
  );
  assert.ok(!migratedRaw.includes("legacy-persisted-secret"));
  assert.ok(!migratedRaw.includes("metal"));
  assert.equal(
    migratedManifest?.embeddingRuntime.endpoint,
    "http://127.0.0.1:9/embeddings",
  );

  assert.equal(
    await readFile(join(legacyHome, "manifest.json"), "utf8"),
    sourceManifestBefore,
  );
  const legacyPaths = resolveWorkspaceIndexStoragePaths(legacyHome);
  const legacyEntities = ZVecOpen(legacyPaths.indexPath, { readOnly: true });
  assert.equal(legacyEntities.stats.docCount, entityCount);
  legacyEntities.closeSync();

  const modelD = new FakeEmbeddingModel();
  const serviceD = await createZvecGrep({
    root: destinationRoot,
    embeddingModel: modelD,
  });
  const search = await serviceD.context({
    query: "configuration device",
    limit: 5,
  });
  assert.ok(search.items.length > 0);
  for (const item of search.items) {
    assert.ok(
      item.file?.absolutePath !== undefined &&
        physicallyUnder(item.file.absolutePath, destinationRoot),
      "destination must resolve under the migrated workspace",
    );
  }
  await serviceD.close();
});

test("migrated index is unverified: first index reconciles same-stat edits", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-migrate-reconcile-");
  const { sourceRoot, manifest } = await makeSourceWorkspace(t, parent);
  const legacyHome = join(sourceRoot, ".zvec-grep-legacy");
  await buildLegacyHome(sourceRoot, legacyHome, manifest.id);

  const destinationRoot = join(parent, "destination");
  await copySourceFiles(sourceRoot, destinationRoot);
  await migrateWorkspaceIndex({ sourceHome: legacyHome, destinationRoot });

  // Same size, same mtime, different content at the destination.
  const target = join(destinationRoot, "docs", "guide.md");
  const indexedStat = await stat(target);
  const original = await readFile(target, "utf8");
  const replaced = original.replace("Configuration", "CONFIGURATION");
  assert.equal(replaced.length, original.length);
  await writeFile(target, replaced);
  await restoreIndexedMtime(target, indexedStat.mtimeMs);

  const model = new CountingEmbeddingModel();
  const service = await createZvecGrep({
    root: destinationRoot,
    embeddingModel: model,
  });
  const result = await service.index();
  assert.ok(
    result.filesModified >= 1,
    `migrated index must reconcile content on first index, got ${JSON.stringify(
      { modified: result.filesModified },
    )}`,
  );
  await service.close();
});

test("migration rejects a cross-file inventory", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-migrate-corrupt-");
  const { sourceRoot, manifest } = await makeSourceWorkspace(t, parent);
  const legacyHome = join(sourceRoot, ".zvec-grep-legacy");
  await buildLegacyHome(sourceRoot, legacyHome, manifest.id, {
    mutateInventory: (entityIds, { legacyFragmentIds }) => [
      // Point one file's inventory at the other file's public fragment.
      [...legacyFragmentIds.values()][0],
      ...entityIds.slice(1),
    ],
  });

  const destinationRoot = join(parent, "destination");
  await copySourceFiles(sourceRoot, destinationRoot);
  await assert.rejects(
    () =>
      migrateWorkspaceIndex({
        sourceHome: legacyHome,
        destinationRoot,
      }),
    /failed verification|inventoriesExact.*false/i,
  );
  // The rejected conversion publishes nothing; only the reservation's lock
  // scaffolding remains in the destination home.
  const homeEntries = await readdir(join(destinationRoot, ".zvec-grep"));
  assert.ok(!homeEntries.includes("manifest.json"));
  assert.ok(!homeEntries.includes("files.zvec"));
  assert.ok(!homeEntries.includes("index.zvec"));
  assert.ok(
    !homeEntries.some((entry) => entry.startsWith("staging-")),
    "no staging residue may remain",
  );
});

test("migration cleans up handles and staging after interruption, and retries", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-migrate-interrupt-");
  const { sourceRoot, manifest } = await makeSourceWorkspace(t, parent);
  const legacyHome = join(sourceRoot, ".zvec-grep-legacy");
  await buildLegacyHome(sourceRoot, legacyHome, manifest.id);

  const destinationRoot = join(parent, "destination");
  await copySourceFiles(sourceRoot, destinationRoot);

  const fdsBefore = fdCount();
  await assert.rejects(() =>
    migrateWorkspaceIndex({
      sourceHome: legacyHome,
      destinationRoot,
      onProgress: (stage) => {
        if (stage === "write") {
          throw new Error("injected interruption");
        }
      },
    }),
  );
  if (fdsBefore !== null) {
    assert.equal(fdCount(), fdsBefore, "no native descriptors may leak");
  }
  const homeEntriesAfterAbort = await readdir(
    join(destinationRoot, ".zvec-grep"),
  );
  assert.ok(!homeEntriesAfterAbort.includes("manifest.json"));
  assert.ok(
    !homeEntriesAfterAbort.some((entry) => entry.startsWith("staging-")),
    "no staging residue may remain after interruption",
  );

  // The source remains readable, and a retry succeeds.
  const legacyPaths = resolveWorkspaceIndexStoragePaths(legacyHome);
  const legacyEntities = ZVecOpen(legacyPaths.indexPath, { readOnly: true });
  assert.ok(legacyEntities.stats.docCount > 0);
  legacyEntities.closeSync();

  const result = await migrateWorkspaceIndex({
    sourceHome: legacyHome,
    destinationRoot,
  });
  assert.equal(result.verification.countsMatch, true);
});

test("migration claims an empty unreserved destination but rejects an indexed one", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-migrate-claim-");
  const { sourceRoot, manifest } = await makeSourceWorkspace(t, parent);
  const legacyHome = join(sourceRoot, ".zvec-grep-legacy");
  await buildLegacyHome(sourceRoot, legacyHome, manifest.id);

  // An empty, unreserved destination directory is claimed by the protocol.
  const emptyRoot = join(parent, "empty-destination");
  await mkdir(join(emptyRoot, ".zvec-grep"), { recursive: true });
  const claimed = await migrateWorkspaceIndex({
    sourceHome: legacyHome,
    destinationRoot: emptyRoot,
  });
  assert.equal(claimed.verification.countsMatch, true);
  assert.ok(
    await readdir(join(emptyRoot, ".zvec-grep")).then((entries) =>
      entries.includes("manifest.json"),
    ),
  );

  // A destination that already holds an index is rejected, source untouched.
  const sourceManifestBefore = await readFile(
    join(legacyHome, "manifest.json"),
    "utf8",
  );
  await assert.rejects(
    () =>
      migrateWorkspaceIndex({
        sourceHome: legacyHome,
        destinationRoot: emptyRoot,
      }),
    /Destination already contains a workspace index/,
  );
  assert.equal(
    await readFile(join(legacyHome, "manifest.json"), "utf8"),
    sourceManifestBefore,
  );
});

test("migration rejects a legacy home marked INCOMPLETE", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-migrate-marked-");
  const { sourceRoot, manifest } = await makeSourceWorkspace(t, parent);
  const legacyHome = join(sourceRoot, ".zvec-grep-legacy");
  await buildLegacyHome(sourceRoot, legacyHome, manifest.id);

  // A crashed operation's durable marker blocks migration even with no
  // writer lock present; the guard runs before any manifest read.
  await writeFile(
    join(legacyHome, "INCOMPLETE"),
    `${JSON.stringify({ token: "crashed", operation: "index.migrate" })}\n`,
  );

  const destinationRoot = join(parent, "destination");
  await copySourceFiles(sourceRoot, destinationRoot);
  await assert.rejects(
    migrateWorkspaceIndex({ sourceHome: legacyHome, destinationRoot }),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.MANIFEST.INCOMPLETE_DESTINATION",
  );
  assert.ok(!(await readdir(destinationRoot)).includes(".zvec-grep"));
});

test("migration rejects a legacy home whose INCOMPLETE marker is a dangling symlink", async (t) => {
  ZVecInitialize({ logLevel: ZVecLogLevel.WARN });
  const parent = await createTemporaryDirectory(t, "zg-migrate-dangling-");
  const { sourceRoot, manifest } = await makeSourceWorkspace(t, parent);
  const legacyHome = join(sourceRoot, ".zvec-grep-legacy");
  await buildLegacyHome(sourceRoot, legacyHome, manifest.id);
  await symlink(join(parent, "no-such-target"), join(legacyHome, "INCOMPLETE"));

  const destinationRoot = join(parent, "destination");
  await copySourceFiles(sourceRoot, destinationRoot);
  await assert.rejects(
    migrateWorkspaceIndex({ sourceHome: legacyHome, destinationRoot }),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.MANIFEST.INCOMPLETE_DESTINATION",
  );
  assert.ok(!(await readdir(destinationRoot)).includes(".zvec-grep"));
});
