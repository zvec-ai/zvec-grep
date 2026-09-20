import assert from "node:assert/strict";
import test from "node:test";
import {
  formatRemoteEmbeddingAuthorizationPrompt,
  remoteEmbeddingDisclosureData,
} from "../../dist/authorization/prompt.js";

test("Remote Embedding prompts use an action-first layered layout", () => {
  const message = formatRemoteEmbeddingAuthorizationPrompt({
    workspaceRoots: ["/Users/example/workspace/zvec-1"],
    provider: "qwen",
    model: "text-embedding-v4",
    endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings",
    data: remoteEmbeddingDisclosureData({
      queryText: false,
      workspaceContent: "selected",
    }),
  });

  assert.equal(
    message,
    [
      "Remote Embedding authorization",
      "",
      "Send selected workspace files?",
      "",
      "  From  zvec-1",
      "  To    qwen/text-embedding-v4",
      "        dashscope.aliyuncs.com",
      "",
      "API charges may apply.",
    ].join("\n"),
  );
  assert.doesNotMatch(message, /compatible-mode/);
  assert.doesNotMatch(message, /\/Users\/example/);
  assert.deepEqual(
    remoteEmbeddingDisclosureData({
      queryText: false,
      workspaceContent: "full",
    }),
    ["selected workspace files"],
  );
});

test("Remote Embedding prompts summarize combined disclosures and notes", () => {
  const message = formatRemoteEmbeddingAuthorizationPrompt({
    workspaceRoots: ["/workspace/one", "/workspace/two", "/workspace/three"],
    provider: "qwen",
    model: "text-embedding-v4",
    endpoint: "dashscope.aliyuncs.com/embeddings",
    data: remoteEmbeddingDisclosureData({
      queryText: true,
      workspaceContent: "changed",
    }),
    note: "Files are read only after approval.",
  });

  assert.match(message, /Send query text and changed workspace files\?/);
  assert.match(message, /From\s+one, two \+1/);
  assert.match(message, /dashscope\.aliyuncs\.com/);
  assert.match(message, /Files are read only after approval\./);
});
