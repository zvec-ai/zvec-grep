import assert from "node:assert/strict";
import test from "node:test";
import { indexChunkOptions } from "../../dist/engine/pipeline/indexing/input-budget.js";
import { vectorContentForFragment } from "../../dist/engine/extraction/vector-content.js";

test("uses the default input estimate for ordinary text", () => {
  assert.deepEqual(
    indexChunkOptions(128_000, "ordinary text ".repeat(20_000)),
    {
      maxChunkChars: 236_800,
      chunkOverlapChars: 35_520,
    },
  );
});

test("uses the conservative input estimate for token-dense text", () => {
  assert.deepEqual(indexChunkOptions(128_000, "<123-456>".repeat(30_000)), {
    maxChunkChars: 128_000,
    chunkOverlapChars: 19_200,
  });
});

test("detects a localized token-dense region", () => {
  const text = `${"ordinary text ".repeat(12_000)}${"<123-456>".repeat(2_000)}`;

  assert.equal(indexChunkOptions(100_000, text).maxChunkChars, 100_000);
});

test("omits input limits when the model does not declare one", () => {
  assert.deepEqual(
    indexChunkOptions(undefined, "<123-456>".repeat(30_000)),
    {},
  );
});

test("embedding content construction does not reject estimated character overflow", () => {
  const fragment = {
    id: "fragment",
    fileId: "file",
    range: {
      kind: "text",
      startLine: 1,
      endLine: 1,
      startOffset: 0,
      endOffset: 5,
    },
    content: { kind: "text", text: "stored content" },
  };
  const embeddingContent = {
    kind: "text",
    text: "x".repeat(256),
  };

  assert.equal(
    vectorContentForFragment(fragment, embeddingContent, 32).text.length,
    256,
  );
});
