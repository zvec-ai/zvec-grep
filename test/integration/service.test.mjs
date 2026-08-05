import assert from "node:assert/strict";
import { access, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { CURRENT_INDEX_VERSION } from "../../dist/engine/types.js";
import { createZvecGrep } from "../../dist/index.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";

class SelectivelyFailingEmbeddingModel extends FakeEmbeddingModel {
  async doEmbed(contents) {
    if (
      contents.some(
        (content) =>
          content.kind === "text" && content.text.includes("FailureNeedle"),
      )
    ) {
      throw new Error("fixture embedding failure");
    }
    return super.doEmbed(contents);
  }
}

test("service indexes, searches, refreshes, and drops a workspace index", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-integration-",
  );
  const root = join(temporaryDirectory, "repo");
  const home = join(temporaryDirectory, "home");
  await mkdir(join(root, "src"), { recursive: true });
  await writeFile(
    join(root, "src", "alpha.ts"),
    "export const UniqueAlphaSymbol = 41;\n",
  );
  await writeFile(join(root, "src", "ignored.log"), "UniqueIgnoredSymbol\n");

  const service = await createZvecGrep({
    root,
    home,
    embeddingModel: new FakeEmbeddingModel(),
  });
  t.after(() => service.close());

  const indexed = await service.index({
    includePaths: ["src/**"],
    excludePaths: ["**/*.log"],
  });
  assert.equal(indexed.filesAdded, 1);
  assert.equal((await service.info()).indexed, true);

  const first = await service.context({
    root,
    query: "UniqueAlphaSymbol",
    limit: 5,
  });
  assert.ok(
    first.items.some((item) => item.file.relativePath.endsWith("alpha.ts")),
  );
  assert.equal(
    first.items.some((item) => item.file.relativePath.endsWith("ignored.log")),
    false,
  );

  await writeFile(
    join(root, "src", "alpha.ts"),
    "export const UniqueUpdatedSymbol = 42;\n",
  );
  const refreshed = await service.context({
    root,
    query: "UniqueUpdatedSymbol",
    limit: 5,
  });
  assert.ok(
    refreshed.items.some((item) => item.file.relativePath.endsWith("alpha.ts")),
  );

  assert.equal(await service.dropIndex(), true);
  assert.equal(await service.dropIndex(), false);
  assert.equal((await service.info()).indexed, false);
});

test("service optionally fuses independent query groups into one result list", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-fused-context-",
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "alpha.ts"), "export const AlphaNeedle = 1;\n");
  await writeFile(join(root, "beta.ts"), "export const BetaNeedle = 2;\n");

  const service = await createZvecGrep({
    root,
    embeddingModel: new FakeEmbeddingModel(),
  });
  t.after(() => service.close());
  await service.index();

  const routes = [
    { mode: "fts", query: "AlphaNeedle" },
    { mode: "fts", query: "BetaNeedle" },
  ];
  const grouped = await service.context({ routes, limit: 1 });
  const fused = await service.context({ routes, limit: 1, fuse: true });

  assert.equal(grouped.items.length, 2);
  assert.equal(fused.items.length, 1);
  assert.equal(fused.diagnostics.index?.routes.length, 2);
});

test("workspace rebuild recreates unsupported index metadata", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-version-rebuild-",
  );
  const root = join(temporaryDirectory, "repo");
  const workspaceHome = join(root, ".zvec-grep");
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "legacy.ts"), "export const LegacyNeedle = 42;\n");

  let service = await createZvecGrep({
    root,
    embeddingModel: new FakeEmbeddingModel(),
  });
  t.after(async () => {
    await service.close();
  });
  await service.index();
  await service.close();

  const manifestPath = join(workspaceHome, "manifest.json");
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  await writeFile(
    manifestPath,
    `${JSON.stringify({ ...manifest, indexVersion: 999 }, null, 2)}\n`,
  );

  const filesMarker = join(workspaceHome, "files.zvec", "legacy-marker");
  const indexMarker = join(workspaceHome, "index.zvec", "legacy-marker");
  const authorizationPath = join(workspaceHome, "authorization.json");
  await writeFile(filesMarker, "legacy");
  await writeFile(indexMarker, "legacy");
  await writeFile(authorizationPath, "preserve");

  service = await createZvecGrep({
    root,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await assert.rejects(
    service.info(),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.COLLECTION.INDEX_VERSION_MISMATCH" &&
      error.context.includes("zg index --rebuild"),
  );

  await service.index({ rebuild: true });
  const rebuilt = await service.info();
  assert.equal(rebuilt.collection?.indexVersion, CURRENT_INDEX_VERSION);
  await assert.rejects(access(filesMarker), { code: "ENOENT" });
  await assert.rejects(access(indexMarker), { code: "ENOENT" });
  assert.equal(await readFile(authorizationPath, "utf8"), "preserve");

  const result = await service.context({
    routes: [{ mode: "fts", query: "LegacyNeedle" }],
    autoUpdate: false,
  });
  assert.ok(result.items.length > 0);
});

test("service records failed files, retries them, deletes stale records, and rebuilds", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-index-failures-",
  );
  const root = join(temporaryDirectory, "repo");
  const home = join(temporaryDirectory, "home");
  await mkdir(root, { recursive: true });
  const goodPath = join(root, "good.ts");
  const failingPath = join(root, "failing.ts");
  await writeFile(goodPath, "export const GoodNeedle = 1;\n");
  await writeFile(failingPath, "export const FailureNeedle = 2;\n");

  const service = await createZvecGrep({
    root,
    home,
    embeddingModel: new SelectivelyFailingEmbeddingModel(),
  });
  t.after(() => service.close());

  const completedFiles = [];
  const retryScanningProgress = [];
  let retryStarted = false;
  await assert.rejects(
    service.index({
      embeddingConcurrency: 2,
      onProgress: (progress) => {
        if (
          progress.phase === "scanning" &&
          progress.detail?.toLowerCase().includes("retry")
        ) {
          retryStarted = true;
        }
        if (retryStarted && progress.phase === "scanning") {
          retryScanningProgress.push(progress);
        }
        if (progress.filesIndexed !== undefined) {
          completedFiles.push(
            progress.filesIndexed - (progress.filesFailed ?? 0),
          );
        }
      },
    }),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED" &&
      /failing\.ts/.test(error.context),
  );
  assert.ok(
    completedFiles.every(
      (completed, index) =>
        index === 0 || completed >= completedFiles[index - 1],
    ),
  );
  assert.ok(retryScanningProgress.length > 0);
  assert.ok(
    retryScanningProgress.every(
      (progress) =>
        progress.filesIndexed !== undefined &&
        progress.filesTotal !== undefined,
    ),
  );
  const failedStatus = await service.info();
  assert.equal(failedStatus.status?.filesFailed, 1);
  assert.equal(failedStatus.status?.filesIndexed, 1);
  assert.match(
    failedStatus.status?.failedFiles[0].indexStatus?.error ?? "",
    /fixture embedding failure/,
  );

  await writeFile(failingPath, "export const RecoveredNeedle = 3;\n");
  const retried = await service.index();
  assert.equal(retried.filesFailed, 0);
  assert.equal(retried.filesPending + retried.filesModified >= 1, true);
  assert.equal((await service.info()).status?.filesIndexed, 2);

  await rm(goodPath);
  const deleted = await service.index();
  assert.equal(deleted.filesDeleted, 1);
  assert.equal((await service.info()).status?.filesIndexed, 1);

  const rebuilt = await service.index({ rebuild: true });
  assert.equal(rebuilt.filesScanned, 1);
  assert.equal(rebuilt.filesAdded, 1);
});
