import assert from "node:assert/strict";
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { ZVecOpen } from "@zvec/zvec";
import { createZvecGrep } from "../../dist/index.js";
import {
  currentCodeExtractionVersion,
  extractForIndexing,
} from "../../dist/engine/extraction/index.js";
import { createWorkspaceIndexStorage } from "../../dist/engine/storage/index.js";
import { resolveWorkspaceIndexStoragePaths } from "../../dist/engine/storage/layout.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";

class RecordingModel extends FakeEmbeddingModel {
  inputs = [];
  failTimeoutUpgrade = false;
  async doEmbed(contents) {
    this.inputs.push(...contents.map((content) => content.text));
    if (
      this.failTimeoutUpgrade &&
      contents.some((content) => content.text.includes("TIMEOUT_SETTINGS"))
    )
      throw new Error("simulated extraction upgrade failure");
    return super.doEmbed(contents);
  }
}

async function legacyFixture(t) {
  const root = await createTemporaryDirectory(t, "zvec-extraction-upgrade-");
  await writeFile(
    join(root, "policy.ts"),
    "const retry_policy_config = [100, 200];\nexport function execute(value) { return value; }\n",
  );
  await writeFile(
    join(root, "timeouts.py"),
    "TIMEOUT_SETTINGS = [30, 60]\ndef execute(value):\n    return value\n",
  );
  await writeFile(
    join(root, "guide.md"),
    "# Documentation sentinel\nNo embedding upgrade is needed here.\n",
  );
  const model = new RecordingModel();
  const initial = await createZvecGrep({
    root,
    embeddingModel: model,
    embeddingModelOwnership: "borrowed",
  });
  await initial.index();
  const info = await initial.info();
  await initial.close();
  const storage = createWorkspaceIndexStorage({
    storagePath: info.home,
    readOnly: false,
    embedding: info.workspaceIndex.embedding,
  });
  const original = storage.listFiles();
  try {
    for (const file of original.filter((file) => file.kind === "code")) {
      const text = await readFile(file.absolutePath, "utf8");
      // The old extractor emitted only these structured functions, omitting
      // the top-level values. Seed exactly that representation, without a version.
      const fragments = (await extractForIndexing({ kind: "text", file, text }))
        .map(({ fragment }) => fragment)
        .filter((fragment) => fragment.metadata);
      const { vectors } = await model.embed(
        fragments.map((fragment) => fragment.content),
      );
      storage.replaceFile(
        file,
        fragments.map((fragment, index) => ({
          fragment,
          vector: vectors[index],
        })),
      );
    }
  } finally {
    storage.close();
  }
  const filesPath = resolveWorkspaceIndexStoragePaths(info.home).filesPath;
  // Test-owned fixture: remove the newly introduced nullable column so the
  // reader/writer migration is exercised against the actual old native schema.
  const native = ZVecOpen(filesPath, { readOnly: false });
  try {
    native.dropColumnSync("extraction_version");
  } finally {
    native.closeSync();
  }
  model.inputs = [];
  return { root, model, info, original, filesPath };
}

function hasVersionColumn(path) {
  const native = ZVecOpen(path, { readOnly: true });
  try {
    return native.schema
      .fields()
      .some((field) => field.name === "extraction_version");
  } finally {
    native.closeSync();
  }
}

test("legacy code refreshes incrementally without rebuilding storage or reembedding unchanged documents", async (t) => {
  const { root, model, info, original, filesPath } = await legacyFixture(t);
  const service = await createZvecGrep({ root, embeddingModel: model });
  t.after(() => service.close());
  assert.equal(hasVersionColumn(filesPath), false);
  const before = await service.info();
  assert.equal(before.indexed, true, "old indexes remain readable");
  assert.equal(before.status.filesModified, 2);
  assert.equal(
    hasVersionColumn(filesPath),
    false,
    "status reads must not mutate the schema",
  );
  const old = await service.context({
    routes: [{ mode: "fts", query: "retry_policy_config" }],
    autoUpdate: false,
  });
  assert.equal(old.items.length, 0);
  const partial = await service.index({
    changedPaths: [join(root, "policy.ts")],
  });
  assert.equal(partial.filesModified, 1);
  assert.equal(hasVersionColumn(filesPath), true);
  assert.equal(
    (await service.info()).status.filesModified,
    1,
    "only the unmigrated code remains stale",
  );
  const migratedPolicyCalls = model.inputs.filter((text) =>
    text.includes("retry_policy_config"),
  ).length;
  model.failTimeoutUpgrade = true;
  await assert.rejects(
    service.index({ changedPaths: [join(root, "timeouts.py")] }),
  );
  assert.equal((await service.info()).status.filesFailed, 1);
  model.failTimeoutUpgrade = false;
  const current = await service.context({
    routes: [{ mode: "fts", query: "TIMEOUT_SETTINGS" }],
    autoUpdate: true,
  });
  assert.ok(
    current.items.some((item) => item.file.relativePath === "timeouts.py"),
  );
  assert.equal((await service.info()).status.filesModified, 0);
  assert.equal(
    model.inputs.filter((text) => text.includes("retry_policy_config")).length,
    migratedPolicyCalls,
    "a failed migration must not reembed files already upgraded successfully",
  );
  assert.ok(
    !model.inputs.some((text) => text.includes("Documentation sentinel")),
  );
  const storage = createWorkspaceIndexStorage({
    storagePath: info.home,
    readOnly: true,
  });
  try {
    for (const file of storage.listFiles()) {
      assert.equal(
        file.contentHash,
        original.find((old) => old.id === file.id).contentHash,
      );
      if (file.kind === "code")
        assert.equal(
          file.indexStatus.extractionVersion,
          currentCodeExtractionVersion(file.format),
        );
    }
  } finally {
    storage.close();
  }
  const inputsBefore = model.inputs.length;
  const unchanged = await service.index();
  assert.equal(unchanged.filesModified, 0);
  assert.equal(unchanged.filesUnchanged, 3);
  assert.equal(model.inputs.length, inputsBefore);
});

test("version-one JavaScript bindings upgrade without reembedding unaffected Python", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-binding-upgrade-");
  const text =
    'const example = "sendFile(path)";\nres.sendFile = function(path) { return path; };\n';
  await writeFile(join(root, "response.js"), text);
  await writeFile(
    join(root, "unchanged.py"),
    "def python_sentinel(value):\n    return value\n",
  );
  const model = new RecordingModel();
  const initial = await createZvecGrep({
    root,
    embeddingModel: model,
    embeddingModelOwnership: "borrowed",
  });
  await initial.index();
  const info = await initial.info();
  await initial.close();
  const storage = createWorkspaceIndexStorage({
    storagePath: info.home,
    readOnly: false,
    embedding: info.workspaceIndex.embedding,
  });
  const original = storage.listFiles();
  try {
    const file = original.find((file) => file.relativePath === "response.js");
    // Version one preserved this source but did not recognize property-assigned
    // functions. Seed a source-only window with the actual old version marker.
    const fragments = (
      await extractForIndexing({
        kind: "text",
        file: { ...file, kind: "text", format: "text" },
        text,
      })
    ).map(({ fragment }) => fragment);
    assert.ok(fragments.every((fragment) => !fragment.metadata));
    const { vectors } = await model.embed(
      fragments.map((fragment) => fragment.content),
    );
    storage.replaceFile(
      file,
      fragments.map((fragment, index) => ({
        fragment,
        vector: vectors[index],
      })),
      { extractionVersion: 1 },
    );
    await storage.finalizeWrites();
  } finally {
    storage.close();
  }
  model.inputs = [];
  const service = await createZvecGrep({ root, embeddingModel: model });
  t.after(() => service.close());
  assert.equal((await service.info()).status.filesModified, 1);
  const before = await service.context({
    routes: [{ mode: "fts", query: "sendFile" }],
    autoUpdate: false,
  });
  assert.ok(before.items.length);
  assert.ok(
    before.items.every((item) => item.metadata?.symbolName !== "sendFile"),
  );
  const after = await service.context({
    routes: [{ mode: "fts", query: "sendFile" }],
    autoUpdate: true,
  });
  assert.equal(after.items[0].metadata.symbolName, "sendFile");
  assert.equal(after.items[0].metadata.scope, "res");
  assert.ok(model.inputs.some((text) => text.includes("res.sendFile")));
  assert.ok(model.inputs.every((text) => !text.includes("python_sentinel")));
  const current = createWorkspaceIndexStorage({
    storagePath: info.home,
    readOnly: true,
  });
  try {
    for (const file of current.listFiles()) {
      const old = original.find((old) => old.id === file.id);
      assert.equal(file.contentHash, old.contentHash);
      assert.equal(
        file.indexStatus.extractionVersion,
        file.format === "javascript" ? 2 : 1,
      );
      if (file.format === "python")
        assert.deepEqual(file.indexStatus, old.indexStatus);
    }
  } finally {
    current.close();
  }
  assert.deepEqual(
    (await service.info()).workspaceIndex.embedding,
    info.workspaceIndex.embedding,
  );
  const inputsBefore = model.inputs.length;
  const unchanged = await service.index();
  assert.equal(unchanged.filesModified, 0);
  assert.equal(unchanged.filesUnchanged, 2);
  assert.equal(model.inputs.length, inputsBefore);
});
