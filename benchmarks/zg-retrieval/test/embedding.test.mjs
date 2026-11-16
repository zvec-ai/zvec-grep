import assert from "node:assert/strict";
import test from "node:test";
import {
  assertCudaEvidence,
  embeddingModel,
  embeddingRuntime,
  REMOTE_EMBEDDING_MODEL,
} from "../core/embedding.mjs";
import { loadSuite } from "../core/suite.mjs";

test("CUDA probes require graph placement and strict index logs require the provider", () => {
  const provider = "Transformers ONNX cuda embedding execution provider selected";
  const placement = "placed on [CUDAExecutionProvider]. Number of nodes: 12";
  assert.doesNotThrow(() => assertCudaEvidence(provider + "\n" + placement));
  assert.throws(() => assertCudaEvidence(provider), /graph-placement evidence/);
  assert.doesNotThrow(() => assertCudaEvidence(provider, { requireGraphPlacement: false }));
  assert.throws(() => assertCudaEvidence(placement, { requireGraphPlacement: false }), /did not select/);
  assert.throws(() => assertCudaEvidence(provider + "\nfalling back to CPU", { requireGraphPlacement: false }), /fell back to CPU/);
});

test("embedding profile keeps the configured local model or selects Qwen remotely", () => {
  assert.equal(
    embeddingModel("local/potion-code-16m-v2", "local"),
    "local/potion-code-16m-v2",
  );
  assert.equal(
    embeddingModel("local/potion-multilingual-128m", "remote"),
    REMOTE_EMBEDDING_MODEL,
  );
  assert.throws(
    () => embeddingModel("local/potion-code-16m-v2", "other"),
    /unknown embedding profile/,
  );
});

test("the selected model is part of the frozen SWE-QA protocol identity", async () => {
  const local = await loadSuite({ embedding: "local" });
  const remote = await loadSuite({ embedding: "remote" });
  assert.equal(local.protocol.model, "local/potion-code-16m-v2");
  assert.equal(remote.protocol.model, REMOTE_EMBEDDING_MODEL);
  assert.notEqual(local.identity.protocol, remote.identity.protocol);
});

test("remote embedding uses one explicit HTTPS endpoint for indexing and MCP consent", () => {
  const endpoint = "https://example.test/compatible-mode/v1/embeddings";
  const runtime = embeddingRuntime(REMOTE_EMBEDDING_MODEL, {
    ZVEC_GREP_API_KEY: "test-key",
    ZVEC_GREP_ENDPOINT: endpoint,
  });
  assert.equal(runtime.remote, true);
  assert.deepEqual(runtime.indexArguments, [
    "--endpoint",
    endpoint,
    "--allow-remote",
  ]);
  assert.deepEqual(runtime.grantArguments("/workspace"), [
    "--auth",
    "grant",
    "/workspace",
    "--capability",
    "embedding",
    "--scope",
    "workspace",
    "--embedding",
    REMOTE_EMBEDDING_MODEL,
    "--endpoint",
    endpoint,
  ]);
  assert.ok(!JSON.stringify(runtime).includes("test-key"));
});

test("remote embedding refuses missing credentials and insecure destinations", () => {
  assert.throws(
    () => embeddingRuntime(REMOTE_EMBEDDING_MODEL, {}),
    /ZVEC_GREP_API_KEY/,
  );
  assert.throws(
    () =>
      embeddingRuntime(REMOTE_EMBEDDING_MODEL, {
        ZVEC_GREP_API_KEY: "test-key",
      }),
    /ZVEC_GREP_ENDPOINT/,
  );
  assert.throws(
    () =>
      embeddingRuntime(REMOTE_EMBEDDING_MODEL, {
        ZVEC_GREP_API_KEY: "test-key",
        ZVEC_GREP_ENDPOINT: "http://example.test/embeddings",
      }),
    /HTTPS/,
  );
});
test("matrix model override accepts the local allowlist and rejects cross-profile use", () => {
  const configured = "local/potion-code-16m-v2";
  const override = "local/bekko-256-int8";
  assert.equal(embeddingModel(configured, "local", override), override);
  assert.equal(embeddingModel(configured, "local", ""), configured);
  assert.throws(
    () => embeddingModel(configured, "remote", override),
    /requires local/,
  );
  assert.throws(
    () => embeddingModel(configured, "local", configured),
    /unsupported matrix model/,
  );
});
