import assert from "node:assert/strict";
import {
  mkdir,
  mkdtemp,
  rm,
  symlink,
  unlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { BaseEmbeddingModel } from "../dist/engine/models/embeddings.js";
import {
  scanDirectoryPath,
  scanFilePath,
} from "../dist/engine/pipeline/indexing/scanner/index.js";
import { createZvecGrep } from "../dist/index.js";

test("path scanners rebuild gitignore rules and stay inside the requested subtree", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-path-scan-"),
  );
  const root = join(temporaryDirectory, "repo");
  const src = join(root, "src");
  const other = join(root, "other");
  await mkdir(src, { recursive: true });
  await mkdir(other);
  await writeFile(join(root, ".gitignore"), "src/ignored.ts\n");
  await writeFile(join(src, "kept.ts"), "export const kept = true;\n");
  await writeFile(join(src, "ignored.ts"), "export const ignored = true;\n");
  await writeFile(join(other, "outside.ts"), "export const outside = true;\n");
  const rootPaths = [{ absolutePath: root, recursive: true }];
  try {
    const ignored = await scanFilePath(
      "workspace-index",
      rootPaths,
      join(src, "ignored.ts"),
    );
    assert.equal(ignored.files.length, 0);
    const subtree = await scanDirectoryPath("workspace-index", rootPaths, src);
    assert.deepEqual(
      subtree.files.map((file) => file.relativePath),
      ["src/kept.ts"],
    );
    await writeFile(join(root, ".gitignore"), "");
    const included = await scanFilePath(
      "workspace-index",
      rootPaths,
      join(src, "ignored.ts"),
    );
    assert.equal(included.files[0].relativePath, "src/ignored.ts");
  } finally {
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("path scanners do not follow symlinks outside the indexed root", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-path-symlink-"),
  );
  const root = join(temporaryDirectory, "repo");
  const external = join(temporaryDirectory, "external");
  await mkdir(root);
  await mkdir(external);
  await writeFile(join(external, "secret.ts"), "export const secret = true;\n");
  try {
    await symlink(join(external, "secret.ts"), join(root, "linked.ts"));
    await symlink(external, join(root, "linked-directory"), "dir");
  } catch (error) {
    if (error.code === "EPERM") {
      t.skip("symlink creation is not permitted");
      return;
    }
    throw error;
  }
  const rootPaths = [{ absolutePath: root, recursive: true }];
  try {
    assert.equal(
      (
        await scanFilePath(
          "workspace-index",
          rootPaths,
          join(root, "linked.ts"),
        )
      ).files.length,
      0,
    );
    assert.equal(
      (
        await scanDirectoryPath(
          "workspace-index",
          rootPaths,
          join(root, "linked-directory"),
        )
      ).files.length,
      0,
    );
  } finally {
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("path scanners follow configured symlinks during incremental updates", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-path-follow-"),
  );
  const root = join(temporaryDirectory, "repo");
  const source = join(root, "source");
  await mkdir(source, { recursive: true });
  await writeFile(join(source, "target.ts"), "export const target = true;\n");
  try {
    await symlink(join(source, "target.ts"), join(root, "linked.ts"));
    await symlink(source, join(root, "linked-directory"), "dir");
  } catch (error) {
    if (error.code === "EPERM") {
      t.skip("symlink creation is not permitted");
      return;
    }
    throw error;
  }
  const rootPaths = [{ absolutePath: root, recursive: true, follow: true }];
  try {
    const file = await scanFilePath(
      "workspace-index",
      rootPaths,
      join(root, "linked.ts"),
    );
    assert.deepEqual(
      file.files.map((item) => item.relativePath),
      ["linked.ts"],
    );
    const directory = await scanDirectoryPath(
      "workspace-index",
      rootPaths,
      join(root, "linked-directory"),
    );
    assert.deepEqual(
      directory.files.map((item) => item.relativePath),
      ["linked-directory/target.ts"],
    );
  } finally {
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("changedPaths indexes and deletes only the affected paths", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-path-index-"),
  );
  const root = join(temporaryDirectory, "repo");
  const removedDirectory = join(root, "removed");
  await mkdir(removedDirectory, { recursive: true });
  const changedFile = join(root, "changed.ts");
  const untouchedFile = join(root, "untouched.ts");
  const removedFile = join(removedDirectory, "old.ts");
  await writeFile(changedFile, "export const value = 1;\n");
  await writeFile(untouchedFile, "export const untouched = true;\n");
  await writeFile(removedFile, "export const old = true;\n");
  const model = new CountingEmbeddingModel();
  const service = await createZvecGrep({ root, embeddingModel: model });
  try {
    await service.index();
    model.embeddedTexts.length = 0;
    await writeFile(changedFile, "export const value = 2;\n");
    const changed = await service.index({ changedPaths: [changedFile] });
    assert.equal(changed.filesScanned, 1);
    assert.equal(changed.filesModified, 1);
    assert.ok(model.embeddedTexts.some((text) => text.includes("value = 2")));
    assert.ok(model.embeddedTexts.every((text) => !text.includes("untouched")));

    const addedDirectory = join(root, "added");
    await mkdir(addedDirectory);
    await writeFile(
      join(addedDirectory, "new.ts"),
      "export const newlyAdded = true;\n",
    );
    const added = await service.index({ changedPaths: [addedDirectory] });
    assert.equal(added.filesAdded, 1);

    await unlink(removedFile);
    const deleted = await service.index({ changedPaths: [removedDirectory] });
    assert.equal(deleted.filesDeleted, 1);
    const info = await service.info();
    assert.equal(info.status.filesStored, 3);
  } finally {
    await service.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("indexing applies the model input limit to code, markdown, and text chunks", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-token-chunks-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const nearLimit = `export function nearLimit() { return "${"x".repeat(180)}"; }\n`;
  const oversized = `export function oversized() { return "${"x".repeat(520)}"; }\n`;
  const oversizedPath = join(root, "oversized.ts");
  assert.ok(nearLimit.length > 120);
  assert.ok(nearLimit.length <= 120 * 2);
  assert.ok(oversized.length > 120 * 2);
  await writeFile(join(root, "near-limit.ts"), nearLimit);
  await writeFile(oversizedPath, oversized);
  await writeFile(join(root, "oversized.txt"), "t".repeat(520));
  await writeFile(
    join(root, "oversized.md"),
    `# Heading\n${"m".repeat(520)}\n`,
  );
  const model = new InputLimitedEmbeddingModel();
  let service = await createZvecGrep({ root, embeddingModel: model });

  try {
    await service.index();
    assert.equal(
      model.embeddedTexts.filter((text) =>
        text.includes("symbol: function nearLimit"),
      ).length,
      1,
    );
    assert.ok(
      model.embeddedTexts.filter((text) =>
        text.includes("symbol: function oversized"),
      ).length > 2,
    );
    const textChunks = model.embeddedTexts.filter((text) => /^t+$/.test(text));
    assert.ok(textChunks.length > 2);
    assert.equal(
      textChunks.every((text) => text.length <= 120 * 2),
      true,
    );
    const markdownChunks = model.embeddedTexts.flatMap((text) => {
      const body = text.match(/\n(m+)$/)?.[1];
      return body ? [body] : [];
    });
    assert.ok(markdownChunks.length > 2);
    assert.equal(
      markdownChunks.every((text) => text.length <= 120 * 2),
      true,
    );
    const truncatedFragmentCount = model.embeddedTexts.filter((text) =>
      text.includes("symbol: function oversized"),
    ).length;
    assert.equal(
      (await service.info()).status.fragmentsTruncated,
      truncatedFragmentCount,
    );

    await service.close();
    service = await createZvecGrep({ root, embeddingModel: model });
    assert.equal(
      (await service.info()).status.fragmentsTruncated,
      truncatedFragmentCount,
    );

    await writeFile(
      oversizedPath,
      "export function compact() { return true; }\n",
    );
    await service.index({ changedPaths: [oversizedPath] });
    assert.equal((await service.info()).status.fragmentsTruncated, 0);
  } finally {
    await service.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

class CountingEmbeddingModel extends BaseEmbeddingModel {
  info = {
    reference: "test/counting",
    provider: "test",
    name: "counting",
    dimension: 8,
    metric: "cosine",
    inputKinds: ["text"],
    limits: { maxBatchSize: 64 },
  };
  embeddedTexts = [];

  async doEmbed(contents) {
    return {
      vectors: contents.map((content) => {
        const text = content.kind === "text" ? content.text : "";
        this.embeddedTexts.push(text);
        return [1, 0, 0, 0, 0, 0, 0, 0];
      }),
      truncated: [],
    };
  }
}

class InputLimitedEmbeddingModel extends CountingEmbeddingModel {
  info = {
    reference: "test/counting",
    provider: "test",
    name: "counting",
    dimension: 8,
    metric: "cosine",
    inputKinds: ["text"],
    limits: { maxBatchSize: 64, maxInputTokens: 120 },
  };

  async doEmbed(contents, options) {
    const result = await super.doEmbed(contents, options);
    return {
      vectors: result.vectors,
      truncated: contents.flatMap((content, index) =>
        content.kind === "text" &&
        content.text.includes("symbol: function oversized")
          ? [index]
          : [],
      ),
    };
  }
}
