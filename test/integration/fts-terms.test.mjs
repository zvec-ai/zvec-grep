import assert from "node:assert/strict";
import { join } from "node:path";
import test from "node:test";
import { createWorkspaceIndexStorage } from "../../dist/engine/storage/index.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

test("native FTS recalls words, not shared whitespace, punctuation or query operators", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-fts-terms-");
  const storagePath = join(root, "storage");
  const writer = createWorkspaceIndexStorage({
    storagePath,
    readOnly: false,
    embedding: {
      provider: "local",
      model: "test",
      dimension: 2,
      metric: "cosine",
    },
  });
  const texts = [
    ["echo", "function echo(value) { return value; }"],
    ["policy", "network backoff policy"],
    ["chinese", "网络重试策略"],
    ["identifier", "foo_bar"],
    ["unrelated_identifier", "quux_baz"],
    ["operators", "AND OR NOT"],
    ["prefix", "policyholder"],
  ];
  try {
    for (const [index, [name, text]] of texts.entries()) {
      const id = index.toString(16).repeat(64);
      const file = {
        id,
        absolutePath: join(root, `${name}.txt`),
        relativePath: `${name}.txt`,
        rootPath: root,
        sizeBytes: Buffer.byteLength(text),
        lastModifiedTime: 1,
        kind: "text",
        format: "text",
      };
      writer.replaceFile(file, [
        {
          fragment: {
            id,
            fileId: id,
            content: { kind: "text", text },
            range: {
              kind: "text",
              startLine: 1,
              endLine: 1,
              startOffset: 0,
              endOffset: text.length,
            },
          },
          vector: [1, 0],
        },
      ]);
    }
  } finally {
    writer.close();
  }
  // Existing, already-written indexes must benefit at read time. No schema or
  // embedding migration should be needed to stop whitespace casting FTS votes.
  const storage = createWorkspaceIndexStorage({ storagePath, readOnly: true });
  try {
    const names = (query) =>
      storage
        .searchFts(query, 20)
        .map((hit) => hit.file.relativePath)
        .sort();
    for (const query of [
      "zzzz",
      "zzzz zzzz",
      "zzzz\tzzzz",
      "zzzz\nzzzz",
      "zzzz ()",
      "zzzz _",
      " ",
      "( )",
      '\\ " +',
    ]) {
      assert.deepEqual(names(query), [], JSON.stringify(query));
    }
    for (const query of [
      "network backoff policy",
      "policy?",
      '"policy"',
      "(policy)",
      "policy*",
      "zzzz -policy",
      "C:\\tmp\\policy",
    ]) {
      assert.deepEqual(names(query), ["policy.txt"], query);
    }
    for (const query of ["foo_bar", "FOO_BAR", "foo"]) {
      assert.deepEqual(names(query), ["identifier.txt"], query);
    }
    for (const query of ["网络 重试", "网络重试", "网络释放模型"]) {
      assert.deepEqual(names(query), ["chinese.txt"], query);
    }
    for (const query of ["AND", "OR", "NOT"]) {
      assert.deepEqual(names(query), ["operators.txt"], query);
    }
    assert.deepEqual(names("zzzz AND policy"), ["operators.txt", "policy.txt"]);
    assert.deepEqual(
      storage.searchFts("network backoff policy", 10, {
        fileIds: ["0".repeat(64)],
      }),
      [],
    );
  } finally {
    storage.close();
  }
});
