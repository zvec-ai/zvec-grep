import assert from "node:assert/strict";
import { mkdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
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

test("service indexes, searches, refreshes, and manages named collections", async (t) => {
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

  const named = await service.collections.index("docs", [
    {
      absolutePath: root,
      recursive: true,
      include: ["src/**"],
      exclude: ["**/*.log"],
    },
  ]);
  assert.equal(named.filesAdded, 1);
  assert.equal((await service.collections.list()).length, 1);
  assert.equal((await service.collections.info("docs"))?.name, "docs");
  assert.equal((await service.collections.status("docs"))?.filesIndexed, 1);
  assert.equal(await service.collections.remove("docs"), true);
  assert.equal(await service.collections.remove("docs"), false);
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

  await assert.rejects(
    service.index({ embeddingConcurrency: 2 }),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED" &&
      /failing\.ts/.test(error.context),
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
