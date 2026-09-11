import assert from "node:assert/strict";
import test from "node:test";
import { indexChunkOptions } from "../../dist/engine/pipeline/indexing/input-budget.js";
import { vectorContentForFragment } from "../../dist/engine/extraction/vector-content.js";

test("caps ordinary text at retrieval passage size", () => {
  assert.deepEqual(
    indexChunkOptions(128_000, "ordinary text ".repeat(20_000)),
    {
      maxChunkChars: 3600,
      chunkOverlapChars: 540,
    },
  );
});

test("caps token-dense text at retrieval passage size", () => {
  assert.deepEqual(indexChunkOptions(128_000, "<123-456>".repeat(30_000)), {
    maxChunkChars: 3600,
    chunkOverlapChars: 540,
  });
});

test("detects a localized token-dense region", () => {
  const text = `${"ordinary text ".repeat(12_000)}${"<123-456>".repeat(2_000)}`;

  assert.equal(indexChunkOptions(1024, text).maxChunkChars, 1024);
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

test("respects smaller model input ceilings", () => {
  assert.deepEqual(indexChunkOptions(1024, "ordinary text"), {
    maxChunkChars: 1894,
    chunkOverlapChars: 284,
  });
  assert.deepEqual(indexChunkOptions(1024, "<123-456>".repeat(3000)), {
    maxChunkChars: 1024,
    chunkOverlapChars: 153,
  });
});
