import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { stat, unlink, utimes, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { createZvecGrep } from "../../dist/index.js";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

const oldText = "export const FreshnessOldMarker = 1;\n";
const newText = "export const FreshnessNewMarker = 1;\n";
const endText = "export const FreshnessEndMarker = 1;\n";
const hash = (text) => createHash("sha256").update(text).digest("hex");

test("search invalidations deduplicate groups and bind old indexed identity to newly observed bytes", async (t) => {
  const { service, file } = await fixture(t);
  const stableLine = "export const DedupEvidenceMarker = 2;\n";
  const beforeText = oldText + stableLine;
  const afterText = newText + stableLine;
  const laterText = endText + stableLine;
  await writeFile(file, beforeText);
  const initial = await service.index({ verifySourcePaths: [file] });
  const committed = initial.sourceFreshness.paths[0].indexed;
  assert.equal(committed.contentHash, hash(beforeText));
  const request = {
    routes: [
      { mode: "fts", query: "FreshnessOldMarker" },
      { mode: "fts", query: "DedupEvidenceMarker" },
    ],
    autoUpdate: false,
  };
  const fresh = await service.context(request);
  assert.equal(fresh.groupResults.length, 2);
  assert.ok(fresh.groupResults.every((group) => group.items.length > 0));
  assert.ok(fresh.items.every((item) => item.status === "fresh"));
  assert.equal(fresh.diagnostics.index.sourceInvalidations, undefined);
  const modified = (await stat(file)).mtime;
  await writeFile(file, afterText);
  await utimes(file, modified, modified);
  const stale = await service.context(request);
  assert.ok(stale.groupResults.every((group) => group.items.length > 0));
  assert.ok(stale.items.every((item) => item.status === "possibly_stale"));
  assert.deepEqual(stale.diagnostics.index.sourceInvalidations, [
    {
      indexed: committed,
      reason: "hash_mismatch",
      observedHash: hash(afterText),
      observedSizeBytes: Buffer.byteLength(afterText),
    },
  ]);
  await writeFile(file, laterText);
  await utimes(file, modified, modified);
  const later = await service.context(request);
  assert.equal(later.diagnostics.index.sourceInvalidations.length, 1);
  assert.equal(
    later.diagnostics.index.sourceInvalidations[0].observedHash,
    hash(laterText),
  );
  assert.deepEqual(
    later.diagnostics.index.sourceInvalidations[0].indexed,
    committed,
  );
});

test("full reconciliation forces content checks only for snapshotted known-drift paths", async (t) => {
  const { service, file, root } = await fixture(t);
  const unrelated = join(root, "unrelated.ts");
  const unrelatedOld = "export const UnrelatedOldMarker = 1;\n";
  const unrelatedNew = "export const UnrelatedNewMarker = 1;\n";
  await writeFile(unrelated, unrelatedOld);
  const fixedMtimeSeconds = 1_700_000_000;
  const indexedMetadata = new Map();
  for (const path of [file, unrelated]) {
    await utimes(path, fixedMtimeSeconds, fixedMtimeSeconds);
    const info = await stat(path);
    assert.equal(info.mtimeMs, fixedMtimeSeconds * 1_000);
    indexedMetadata.set(path, { mtimeMs: info.mtimeMs, size: info.size });
  }
  await service.index();
  for (const [path, text] of [
    [file, newText],
    [unrelated, unrelatedNew],
  ]) {
    const before = indexedMetadata.get(path);
    await writeFile(path, text);
    await utimes(path, before.mtimeMs / 1_000, before.mtimeMs / 1_000);
    const after = await stat(path);
    assert.equal(after.mtimeMs, before.mtimeMs);
    assert.equal(after.size, before.size);
  }
  const metadataOnly = await service.info({ includeStatus: true });
  assert.equal(metadataOnly.status.filesModified, 0);
  assert.equal(metadataOnly.status.filesUnchanged, 2);
  const paths = [file];
  const repaired = await service.index({
    verifySourcePaths: paths,
    onProgress() {
      paths.splice(0, paths.length, unrelated);
    },
  });
  assert.equal(repaired.filesModified, 1);
  assert.equal(repaired.filesUnchanged, 1);
  assert.equal(repaired.sourceFreshness.paths.length, 1);
  assert.equal(repaired.sourceFreshness.paths[0].absolutePath, file);
  assert.equal(repaired.sourceFreshness.paths[0].status, "fresh");
  assert.equal(
    repaired.sourceFreshness.paths[0].indexed.contentHash,
    hash(newText),
  );
  const current = await context(service, "FreshnessNewMarker");
  assert.ok(current.items.length > 0);
  assert.ok(current.items.every((item) => item.status === "fresh"));
  const remaining = await context(service, "UnrelatedOldMarker");
  assert.equal(
    remaining.diagnostics.index.sourceInvalidations[0].observedHash,
    hash(unrelatedNew),
  );
});

test("post-index proof rejects bytes edited during embedding rather than trusting job success", async (t) => {
  const entered = Promise.withResolvers();
  const release = Promise.withResolvers();
  let held = false;
  class Model extends FakeEmbeddingModel {
    async doEmbed(contents, options) {
      if (held && options.purpose === "document") {
        entered.resolve();
        await release.promise;
      }
      return super.doEmbed(contents, options);
    }
  }
  const { service, file } = await fixture(t, new Model());
  await service.index();
  await writeFile(file, newText);
  held = true;
  const indexing = service.index({
    changedPaths: [file],
    verifySourcePaths: [file],
  });
  void indexing.catch(() => {});
  try {
    await deadline(entered.promise, "document embedding gate was not reached");
    await writeFile(file, endText);
  } finally {
    release.resolve();
    await indexing;
  }
  const result = await indexing;
  assert.equal(result.filesFailed, 0);
  const proof = result.sourceFreshness.paths[0];
  assert.equal(proof.status, "unverified");
  assert.equal(proof.reason, "hash_mismatch");
  assert.equal(proof.invalidation.indexed.contentHash, hash(newText));
  assert.equal(proof.invalidation.observedHash, hash(endText));
});

test("changed-path deletion and rebuild proofs come from the current committed generation", async (t) => {
  const { service, file } = await fixture(t);
  const initial = await service.index({ verifySourcePaths: [file] });
  await unlink(file);
  const missing = await context(service, "FreshnessOldMarker");
  assert.equal(
    missing.diagnostics.index.sourceInvalidations[0].reason,
    "missing",
  );
  const removed = await service.index({
    changedPaths: [file],
    verifySourcePaths: [file],
  });
  assert.equal(removed.filesDeleted, 1);
  assert.deepEqual(removed.sourceFreshness.paths, [
    { absolutePath: file, status: "absent" },
  ]);
  await writeFile(file, newText);
  const rebuilt = await service.index({
    rebuild: true,
    verifySourcePaths: [file],
  });
  assert.notEqual(
    rebuilt.sourceFreshness.workspaceIndexId,
    initial.sourceFreshness.workspaceIndexId,
  );
  const proof = rebuilt.sourceFreshness.paths[0];
  assert.equal(proof.status, "fresh");
  assert.equal(
    proof.indexed.workspaceIndexId,
    rebuilt.sourceFreshness.workspaceIndexId,
  );
  assert.equal(proof.indexed.contentHash, hash(newText));
});

async function fixture(t, model = new FakeEmbeddingModel()) {
  const root = await createTemporaryDirectory(t, "zvec-source-invalidation-");
  const file = join(root, "example.ts");
  await writeFile(file, oldText);
  const service = await createZvecGrep({ root, embeddingModel: model });
  t.after(() => service.close());
  return { service, file, root };
}

function context(service, query) {
  return service.context({
    routes: [{ mode: "fts", query }],
    autoUpdate: false,
  });
}

async function deadline(promise, message) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(message)), 5_000);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}
