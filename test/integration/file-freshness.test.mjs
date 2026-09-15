import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import fs from "node:fs";
import { stat, unlink, utimes, writeFile } from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { join } from "node:path";
import { setTimeout } from "node:timers/promises";
import test from "node:test";
import { createZvecGrep } from "../../dist/index.js";
import { createWorkspaceIndexStorage } from "../../dist/engine/storage/index.js";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

const oldText = "export const FreshnessOldMarker = 1;\n";
const newText = "export const FreshnessNewMarker = 1;\n";

async function fixture(t, model = new FakeEmbeddingModel(), text = oldText) {
  const root = await createTemporaryDirectory(t, "zvec-file-freshness-");
  const file = join(root, "example.ts");
  await writeFile(file, text);
  const service = await createZvecGrep({ root, embeddingModel: model });
  t.after(() => service.close());
  return { service, file, model };
}

async function search(service, query = "FreshnessOldMarker") {
  const result = await service.context({
    routes: [{ mode: "fts", query }],
    autoUpdate: false,
    limit: 10,
  });
  assert.ok(result.items.length > 0, "the fixture returns indexed evidence");
  return result.items;
}

test("an edit during held embedding does not label old source fresh", async (t) => {
  const entered = Promise.withResolvers();
  const released = Promise.withResolvers();
  class HeldEmbeddingModel extends FakeEmbeddingModel {
    async doEmbed(contents, options) {
      entered.resolve();
      await released.promise;
      return super.doEmbed(contents, options);
    }
  }
  const { service, file } = await fixture(t, new HeldEmbeddingModel());
  const indexing = service.index();
  try {
    await entered.promise;
    await writeFile(file, newText);
    // Keep the write timestamp before the eventual commit on fine-resolution
    // filesystems too; the old commit-time shortcut incorrectly accepts it.
    const modified = (await stat(file)).mtimeMs;
    await setTimeout(Math.max(1, modified - Date.now() + 2));
  } finally {
    released.resolve();
    await indexing;
  }
  const items = await search(service);
  assert.ok(items.every((item) => item.status === "possibly_stale"));
  assert.ok(items.some((item) => item.content.includes("FreshnessOldMarker")));
});

test("content verification detects preserved and backdated mtimes", async (t) => {
  const { service, file } = await fixture(t);
  assert.equal(oldText.length, newText.length);
  const originalMtime = new Date(Date.now() - 60_000);
  await utimes(file, originalMtime, originalMtime);
  await service.index();
  assert.ok((await search(service)).every((item) => item.status === "fresh"));

  await writeFile(file, newText);
  for (const mtime of [
    originalMtime,
    new Date(originalMtime.getTime() - 60_000),
  ]) {
    await utimes(file, mtime, mtime);
    const items = await search(service);
    assert.ok(items.every((item) => item.status === "possibly_stale"));
  }

  await writeFile(file, oldText);
  const futureMtime = new Date(Date.now() + 60_000);
  await utimes(file, futureMtime, futureMtime);
  assert.ok((await search(service)).every((item) => item.status === "fresh"));
  await unlink(file);
  assert.ok(
    (await search(service)).every((item) => item.status === "possibly_stale"),
  );
});

test("changed-path refresh checks bytes even when size and mtime are preserved", async (t) => {
  const { service, file } = await fixture(t);
  const originalMtime = new Date(Date.now() - 60_000);
  await utimes(file, originalMtime, originalMtime);
  await service.index();

  await writeFile(file, newText);
  await utimes(file, originalMtime, originalMtime);
  const refreshed = await service.index({ changedPaths: [file] });
  assert.equal(refreshed.filesModified, 1);
  const items = await search(service, "FreshnessNewMarker");
  assert.ok(items.every((item) => item.status === "fresh"));
  assert.ok(items.some((item) => item.content.includes("FreshnessNewMarker")));

  const unchanged = await service.index({ changedPaths: [file] });
  assert.equal(unchanged.filesModified, 0);
  assert.equal(unchanged.filesUnchanged, 1);
});

test("committed hashes and sizes describe the bytes actually extracted", async (t) => {
  const { service, file } = await fixture(t);
  const extractedText = newText + "// added between diff and source read\n";
  let edited = false;
  await service.index({
    onProgress(progress) {
      if (!edited && progress.detail === "reading example.ts") {
        edited = true;
        fs.writeFileSync(file, extractedText);
      }
    },
  });
  assert.ok(edited, "edit is injected after diff hashing, before source read");
  const info = await service.info();
  const storage = createWorkspaceIndexStorage({
    storagePath: info.home,
    readOnly: true,
  });
  try {
    const [stored] = storage.listFiles();
    assert.equal(stored.sizeBytes, Buffer.byteLength(extractedText));
    assert.equal(
      stored.contentHash,
      createHash("sha256").update(extractedText).digest("hex"),
    );
  } finally {
    storage.close();
  }
  assert.ok(
    (await search(service, "FreshnessNewMarker")).every(
      (item) => item.status === "fresh",
    ),
  );

  await writeFile(file, oldText);
  const futureMtime = new Date(Date.now() + 60_000);
  await utimes(file, futureMtime, futureMtime);
  assert.ok(
    (await search(service, "FreshnessNewMarker")).every(
      (item) => item.status === "possibly_stale",
    ),
  );
});

test("legacy indexed evidence without a source hash is not verified fresh", async (t) => {
  const { service, model } = await fixture(t);
  await service.index();
  const info = await service.info();
  const storage = createWorkspaceIndexStorage({
    storagePath: info.home,
    readOnly: false,
    embedding: info.workspaceIndex.embedding,
  });
  try {
    const [file] = storage.listFiles();
    const entities = storage.listEntitiesByFile(file.id);
    const { vectors } = await model.embed(
      entities.map(({ entity }) => entity.content),
    );
    storage.replaceFile(
      { ...file, contentHash: undefined },
      entities.map(({ entity }, index) => ({
        fragment: entity,
        vector: vectors[index],
      })),
    );
  } finally {
    storage.close();
  }
  assert.ok(
    (await search(service)).every((item) => item.status === "possibly_stale"),
  );
});

test("freshness hashes each returned source once per request, not across requests", async (t) => {
  const text = [
    "export function freshnessFirst() { return 'freshness needle'; }",
    "export function freshnessSecond() { return 'freshness needle'; }",
    "",
  ].join("\n");
  const { service, file } = await fixture(t, new FakeEmbeddingModel(), text);
  await service.index();
  const original = fs.readFileSync;
  let sourceReads = 0;
  const mocked = t.mock.method(fs, "readFileSync", (...args) => {
    if (String(args[0]) === file) sourceReads++;
    return original(...args);
  });
  syncBuiltinESMExports();
  try {
    const request = {
      routes: [
        { mode: "fts", query: "freshness" },
        { mode: "fts", query: "needle" },
      ],
      autoUpdate: false,
      limit: 10,
    };
    const first = await service.context(request);
    assert.ok(first.groupResults.length >= 2);
    assert.ok(first.items.length >= 2);
    assert.ok(first.items.every((item) => item.status === "fresh"));
    assert.equal(sourceReads, 1);

    const originalMtime = (await stat(file)).mtime;
    await writeFile(file, text.replaceAll("needle", "edited"));
    await utimes(file, originalMtime, originalMtime);
    const second = await service.context(request);
    assert.ok(second.items.length >= 2);
    assert.ok(second.items.every((item) => item.status === "possibly_stale"));
    assert.equal(sourceReads, 2, "the next request checks source bytes again");
  } finally {
    mocked.mock.restore();
    syncBuiltinESMExports();
  }
});
