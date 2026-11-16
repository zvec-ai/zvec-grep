import assert from "node:assert/strict";

export const REMOTE_EMBEDDING_MODEL = "qwen/qwen3.7-text-embedding";

const MATRIX_LOCAL_MODELS = new Set([
  "local/bekko-256-int8",
  "local/embeddinggemma-300m",
  "local/qwen3-embedding-0.6b",
  "local/potion-retrieval-32m",
  "local/all-minilm-l6-v2",
  "local/bge-small-en-v1.5",
  "local/multilingual-e5-small",
  "local/gte-modernbert-base",
  "local/jina-embeddings-v2-base-code",
  "local/nomic-embed-text-v1.5",
]);

export function embeddingModel(
  configuredModel,
  profile = "local",
  localModel = process.env.RETRIEVAL_LOCAL_MODEL,
) {
  assert.ok(
    ["local", "remote"].includes(profile),
    `unknown embedding profile: ${profile}`,
  );
  if (localModel) {
    assert.equal(
      profile,
      "local",
      "the local model override requires local embedding",
    );
    assert.ok(
      MATRIX_LOCAL_MODELS.has(localModel),
      `unsupported matrix model: ${localModel}`,
    );
    return localModel;
  }
  return profile === "remote" ? REMOTE_EMBEDDING_MODEL : configuredModel;
}

export function localEmbeddingDevice(configuredDevice) {
  const override = process.env.RETRIEVAL_LOCAL_DEVICE;
  if (!override) return configuredDevice;
  assert.equal(override, "cuda", "unsupported local benchmark device override");
  return override;
}

export function assertCudaEvidence(stderr) {
  assert.doesNotMatch(stderr, /fall(?:ing)? back to CPU/i, "CUDA fell back to CPU");
  assert.ok(
    stderr.includes("Transformers ONNX cuda embedding execution provider selected") &&
      /placed on \[CUDAExecutionProvider\]\. Number of nodes: [1-9]\d*/.test(stderr),
    "no ONNX CUDA graph-placement evidence; see the index log",
  );
}

/** Remote benchmark runs require an explicit destination and a credential. */
export function embeddingRuntime(model, environment = process.env) {
  if (!model.startsWith("qwen/"))
    return { remote: false, indexArguments: [], grantArguments: () => [] };

  assert.ok(
    environment.ZVEC_GREP_API_KEY?.trim(),
    "ZVEC_GREP_API_KEY is required for remote embedding",
  );
  const endpoint = environment.ZVEC_GREP_ENDPOINT?.trim();
  assert.ok(endpoint, "ZVEC_GREP_ENDPOINT is required for remote embedding");
  assert.equal(
    new URL(endpoint).protocol,
    "https:",
    "remote embedding endpoint must use HTTPS",
  );
  return {
    remote: true,
    indexArguments: ["--endpoint", endpoint, "--allow-remote"],
    grantArguments: (root) => [
      "--auth",
      "grant",
      root,
      "--capability",
      "embedding",
      "--scope",
      "workspace",
      "--embedding",
      model,
      "--endpoint",
      endpoint,
    ],
  };
}
