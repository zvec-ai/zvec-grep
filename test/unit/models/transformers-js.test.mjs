import assert from "node:assert/strict";
import { join, resolve } from "node:path";
import test from "node:test";
import { TransformersJsEmbeddingModel } from "../../../dist/engine/models/backends/transformers-js.js";

const MODEL_CACHE_DIRECTORY = resolve("/tmp/model-cache");
const HUGGING_FACE_SNAPSHOT_DIRECTORY = join(
  MODEL_CACHE_DIRECTORY,
  "test",
  "model-ONNX",
  "0123456789abcdef",
);
const MODEL_SCOPE_SNAPSHOT_DIRECTORY = join(
  MODEL_CACHE_DIRECTORY,
  "modelscope",
  "transformers-js",
  "mirror--test-model-ONNX",
  "fedcba9876543210",
);

function entry(overrides = {}) {
  return {
    backend: "transformers-js",
    reference: "local/test-transformer",
    provider: "local",
    model: "test-transformer",
    repo: "test/model-ONNX",
    revision: "0123456789abcdef",
    sources: {
      huggingFace: {
        repo: "test/model-ONNX",
        revision: "0123456789abcdef",
      },
      modelScope: {
        repo: "mirror/test-model-ONNX",
        revision: "fedcba9876543210",
      },
    },
    artifacts: [
      {
        path: "onnx/model_quantized.onnx",
        size: 100,
        sha256: "a".repeat(64),
      },
      {
        path: "tokenizer.json",
        size: 20,
        sha256: "b".repeat(64),
      },
    ],
    dtype: "q8",
    dimension: 3,
    metric: "cosine",
    pooling: "cls",
    normalize: true,
    queryPrefix: "query: ",
    documentPrefix: "passage: ",
    maxInputTokens: 512,
    maxBatchSize: 32,
    ...overrides,
  };
}

function createTokenizer(tokenCount = () => 1) {
  const calls = [];
  return Object.assign(
    async (input, options) => {
      calls.push({ input, options });
      const texts = Array.isArray(input) ? input : [input];
      const tokenCounts = texts.map((text) =>
        Math.min(tokenCount(text), options.max_length),
      );
      const sequenceLength = Math.max(0, ...tokenCounts);
      const inputIds = new BigInt64Array(texts.length * sequenceLength);
      const attentionMask = new BigInt64Array(texts.length * sequenceLength);
      for (let inputIndex = 0; inputIndex < texts.length; inputIndex++) {
        const offset = inputIndex * sequenceLength;
        for (
          let tokenIndex = 0;
          tokenIndex < tokenCounts[inputIndex];
          tokenIndex++
        ) {
          attentionMask[offset + tokenIndex] = 1n;
        }
      }
      return {
        input_ids: {
          data: inputIds,
          dims: [texts.length, sequenceLength],
        },
        attention_mask: {
          data: attentionMask,
          dims: [texts.length, sequenceLength],
        },
      };
    },
    { calls, model_max_length: 4096 },
  );
}

function createArtifactResolver(
  directory = resolve("/tmp/resolved-transformers-model"),
) {
  return async ({ sources, artifacts }) => ({
    source: sources[0],
    directory,
    paths: Object.fromEntries(
      artifacts.map((artifact) => [
        artifact.path,
        join(directory, artifact.path),
      ]),
    ),
  });
}

test("Transformers.js resolves artifacts before loading a local-only pipeline", async () => {
  const loads = [];
  const resolutions = [];
  const calls = [];
  const downloadProgress = [];
  let artifactsResolved = false;
  let disposals = 0;
  const extractor = Object.assign(
    async (texts, options) => {
      calls.push({ texts, options });
      return {
        dims: [texts.length, 3],
        data: Float32Array.from(
          texts.flatMap((_, index) => [index + 0.1, index + 0.2, index + 0.3]),
        ),
      };
    },
    {
      tokenizer: createTokenizer(),
      async dispose() {
        disposals++;
      },
    },
  );
  const dependencies = {
    async resolveArtifacts(options) {
      resolutions.push(options);
      options.onDownloadPlan?.(options.artifacts);
      options.onProgress?.({
        model: options.model,
        source: "huggingface",
        artifact: "onnx/model_quantized.onnx",
        downloadedBytes: 25,
        totalBytes: 100,
      });
      options.onProgress?.({
        model: options.model,
        source: "huggingface",
        artifact: "tokenizer.json",
        downloadedBytes: 10,
        totalBytes: 20,
      });
      artifactsResolved = true;
      return {
        source: options.sources[0],
        directory: HUGGING_FACE_SNAPSHOT_DIRECTORY,
        paths: {
          "onnx/model_quantized.onnx": join(
            HUGGING_FACE_SNAPSHOT_DIRECTORY,
            "onnx/model_quantized.onnx",
          ),
          "tokenizer.json": join(
            HUGGING_FACE_SNAPSHOT_DIRECTORY,
            "tokenizer.json",
          ),
        },
      };
    },
    loadRuntime: async () => {
      assert.equal(artifactsResolved, true);
      assert.deepEqual(downloadProgress[0], {
        stage: "preparing",
        model: "local/test-transformer",
      });
      return {
        async pipeline(task, repo, options) {
          loads.push({ task, repo, options });
          return extractor;
        },
      };
    },
  };

  const model = new TransformersJsEmbeddingModel(
    entry(),
    {
      apiKey: "",
      modelCacheDir: MODEL_CACHE_DIRECTORY,
    },
    dependencies,
  );
  assert.deepEqual(
    await model.embed(
      [
        { kind: "text", text: "find auth" },
        { kind: "text", text: "find parser" },
      ],
      {
        purpose: "query",
        onProgress: (progress) => downloadProgress.push(progress),
      },
    ),
    {
      vectors: [
        Array.from(Float32Array.from([0.1, 0.2, 0.3])),
        Array.from(Float32Array.from([1.1, 1.2, 1.3])),
      ],
      truncated: [],
    },
  );
  await model.embed([{ kind: "text", text: "implementation" }]);

  assert.deepEqual(loads, [
    {
      task: "feature-extraction",
      repo: HUGGING_FACE_SNAPSHOT_DIRECTORY,
      options: {
        dtype: "q8",
        local_files_only: true,
      },
    },
  ]);
  assert.equal(resolutions.length, 1);
  const { onDownloadPlan, onProgress, onFallback, ...resolution } =
    resolutions[0];
  assert.equal(typeof onDownloadPlan, "function");
  assert.equal(typeof onProgress, "function");
  assert.equal(typeof onFallback, "function");
  assert.deepEqual(resolution, {
    model: "local/test-transformer",
    sources: [
      {
        kind: "huggingface",
        repo: "test/model-ONNX",
        revision: "0123456789abcdef",
        cacheDirectory: HUGGING_FACE_SNAPSHOT_DIRECTORY,
      },
      {
        kind: "modelscope",
        repo: "mirror/test-model-ONNX",
        revision: "fedcba9876543210",
        cacheDirectory: MODEL_SCOPE_SNAPSHOT_DIRECTORY,
      },
    ],
    artifacts: entry().artifacts,
  });
  assert.deepEqual(downloadProgress, [
    {
      stage: "preparing",
      model: "local/test-transformer",
    },
    {
      stage: "downloading",
      model: "local/test-transformer",
      downloadedBytes: 25,
      totalBytes: 120,
    },
    {
      stage: "downloading",
      model: "local/test-transformer",
      downloadedBytes: 35,
      totalBytes: 120,
    },
    {
      stage: "ready",
      model: "local/test-transformer",
    },
  ]);
  assert.equal(extractor.tokenizer.model_max_length, 512);
  assert.deepEqual(extractor.tokenizer.calls, [
    {
      input: ["query: find auth", "query: find parser"],
      options: {
        truncation: true,
        max_length: 513,
        padding: true,
      },
    },
    {
      input: ["passage: implementation"],
      options: {
        truncation: true,
        max_length: 513,
        padding: true,
      },
    },
  ]);
  assert.deepEqual(calls, [
    {
      texts: ["query: find auth", "query: find parser"],
      options: {
        pooling: "cls",
        normalize: true,
        truncation: true,
        max_length: 512,
      },
    },
    {
      texts: ["passage: implementation"],
      options: {
        pooling: "cls",
        normalize: true,
        truncation: true,
        max_length: 512,
      },
    },
  ]);

  await model.dispose();
  await model.dispose();
  assert.equal(disposals, 1);
  await assert.rejects(
    model.embed([{ kind: "text", text: "after dispose" }]),
    /disposed/,
  );
});

test("Transformers.js resets progress to the missing ModelScope artifacts on fallback", async () => {
  const loads = [];
  const progress = [];
  const extractor = Object.assign(
    async () => ({ dims: [1, 3], data: new Float32Array(3) }),
    { tokenizer: createTokenizer(), async dispose() {} },
  );
  const dependencies = {
    async resolveArtifacts(options) {
      options.onDownloadPlan?.(options.artifacts);
      options.onProgress?.({
        model: options.model,
        source: "huggingface",
        artifact: "onnx/model_quantized.onnx",
        downloadedBytes: 100,
        totalBytes: 100,
      });
      options.onProgress?.({
        model: options.model,
        source: "huggingface",
        artifact: "tokenizer.json",
        downloadedBytes: 10,
        totalBytes: 20,
      });
      options.onFallback?.("Hugging Face unavailable; using ModelScope.");
      options.onFallback?.("duplicate fallback warning");
      const eventCountBeforePlan = progress.length;
      options.onDownloadPlan?.([options.artifacts[1]]);
      assert.equal(progress.length, eventCountBeforePlan);
      options.onProgress?.({
        model: options.model,
        source: "modelscope",
        artifact: "tokenizer.json",
        downloadedBytes: 0,
        totalBytes: 20,
      });
      options.onProgress?.({
        model: options.model,
        source: "modelscope",
        artifact: "tokenizer.json",
        downloadedBytes: 20,
        totalBytes: 20,
      });
      const source = options.sources[1];
      return {
        source,
        directory: source.cacheDirectory,
        paths: Object.fromEntries(
          options.artifacts.map((artifact) => [
            artifact.path,
            join(source.cacheDirectory, artifact.path),
          ]),
        ),
      };
    },
    loadRuntime: async () => ({
      async pipeline(task, repo, options) {
        loads.push({ task, repo, options });
        return extractor;
      },
    }),
  };
  const model = new TransformersJsEmbeddingModel(
    entry(),
    { apiKey: "", modelCacheDir: MODEL_CACHE_DIRECTORY },
    dependencies,
  );

  await model.embed([{ kind: "text", text: "value" }], {
    onProgress: (event) => progress.push(event),
  });

  assert.deepEqual(loads, [
    {
      task: "feature-extraction",
      repo: MODEL_SCOPE_SNAPSHOT_DIRECTORY,
      options: { dtype: "q8", local_files_only: true },
    },
  ]);
  assert.deepEqual(
    progress.filter((event) => event.stage === "downloading"),
    [
      {
        stage: "downloading",
        model: "local/test-transformer",
        downloadedBytes: 100,
        totalBytes: 120,
      },
      {
        stage: "downloading",
        model: "local/test-transformer",
        downloadedBytes: 110,
        totalBytes: 120,
      },
      {
        stage: "downloading",
        model: "local/test-transformer",
        downloadedBytes: 0,
        totalBytes: 20,
      },
      {
        stage: "downloading",
        model: "local/test-transformer",
        downloadedBytes: 20,
        totalBytes: 20,
      },
    ],
  );
  assert.deepEqual(
    progress.filter((event) => event.stage === "warning"),
    [
      {
        stage: "warning",
        model: "local/test-transformer",
        message: "Hugging Face unavailable; using ModelScope.",
      },
    ],
  );
  await model.dispose();
});

test("Transformers.js keeps cached model loading in the preparation stage", async () => {
  const progress = [];
  const extractor = Object.assign(
    async () => ({ dims: [1, 3], data: new Float32Array(3) }),
    { tokenizer: createTokenizer(), async dispose() {} },
  );
  const model = new TransformersJsEmbeddingModel(
    entry(),
    { apiKey: "" },
    {
      resolveArtifacts: createArtifactResolver(),
      loadRuntime: async () => ({ pipeline: async () => extractor }),
    },
  );

  await model.embed([{ kind: "text", text: "cached" }], {
    onProgress: (event) => progress.push(event),
  });

  assert.deepEqual(progress, [
    { stage: "preparing", model: "local/test-transformer" },
    { stage: "ready", model: "local/test-transformer" },
  ]);
  await model.dispose();
});

test("Transformers.js adapter validates the returned batch tensor", async () => {
  const extractor = Object.assign(
    async () => ({ dims: [1, 2], data: new Float32Array(2) }),
    { tokenizer: createTokenizer(), async dispose() {} },
  );
  const dependencies = {
    resolveArtifacts: createArtifactResolver(),
    loadRuntime: async () => ({
      async pipeline() {
        return extractor;
      },
    }),
  };

  const model = new TransformersJsEmbeddingModel(
    entry(),
    { apiKey: "" },
    dependencies,
  );
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    (error) =>
      error.message === "Transformers.js embedding failed" &&
      error.cause?.message === "Transformers.js returned an unexpected tensor",
  );
  await model.dispose();
});

test("Transformers.js adapter maps Metal to WebGPU", async () => {
  const loads = [];
  const extractor = Object.assign(
    async () => ({ dims: [1, 3], data: new Float32Array(3) }),
    { tokenizer: createTokenizer(), async dispose() {} },
  );
  const dependencies = {
    resolveArtifacts: createArtifactResolver(),
    loadRuntime: async () => ({
      async pipeline(task, repo, options) {
        loads.push({ task, repo, options });
        return extractor;
      },
    }),
  };

  const model = new TransformersJsEmbeddingModel(
    entry(),
    {
      apiKey: "",
      device: "metal",
    },
    dependencies,
  );
  await model.embed([{ kind: "text", text: "value" }]);

  assert.deepEqual(loads[0].options.session_options, {
    executionProviders: ["webgpu"],
  });
  await model.dispose();
});

test("Transformers.js adapter falls back to CPU when GPU initialization fails", async (t) => {
  const providers = [];
  let artifactResolutions = 0;
  const extractor = Object.assign(
    async () => ({ dims: [1, 3], data: new Float32Array(3) }),
    { tokenizer: createTokenizer(), async dispose() {} },
  );
  const resolveArtifacts = createArtifactResolver();
  const dependencies = {
    async resolveArtifacts(options) {
      artifactResolutions++;
      return await resolveArtifacts(options);
    },
    loadRuntime: async () => ({
      async pipeline(_task, _repo, options) {
        const provider = options.session_options?.executionProviders[0];
        providers.push(provider);
        if (provider === "webgpu") {
          throw new Error("GPU unavailable");
        }
        return extractor;
      },
    }),
  };

  const writes = [];
  t.mock.method(process.stderr, "write", (message) => {
    writes.push(String(message));
    return true;
  });
  const model = new TransformersJsEmbeddingModel(
    entry(),
    {
      apiKey: "",
      device: "metal",
    },
    dependencies,
  );
  await model.embed([{ kind: "text", text: "value" }]);

  assert.deepEqual(providers, ["webgpu", "cpu"]);
  assert.equal(artifactResolutions, 1);
  assert.match(writes.join(""), /falling back to CPU/);
  await model.dispose();
});

test("Transformers.js adapter retries on CPU when GPU inference returns invalid values", async (t) => {
  const providers = [];
  let artifactResolutions = 0;
  let activeProvider;
  let gpuDisposals = 0;
  const makeExtractor = (provider) =>
    Object.assign(
      async () => ({
        dims: [1, 3],
        data:
          provider === "webgpu"
            ? new Float32Array([Number.NaN, 0, 0])
            : new Float32Array([1, 2, 3]),
      }),
      {
        tokenizer: createTokenizer(),
        async dispose() {
          if (provider === "webgpu") {
            gpuDisposals++;
          }
        },
      },
    );
  const resolveArtifacts = createArtifactResolver();
  const dependencies = {
    async resolveArtifacts(options) {
      artifactResolutions++;
      return await resolveArtifacts(options);
    },
    loadRuntime: async () => ({
      async pipeline(_task, _repo, options) {
        activeProvider = options.session_options?.executionProviders[0];
        providers.push(activeProvider);
        return makeExtractor(activeProvider);
      },
    }),
  };

  const writes = [];
  t.mock.method(process.stderr, "write", (message) => {
    writes.push(String(message));
    return true;
  });
  const model = new TransformersJsEmbeddingModel(
    entry(),
    {
      apiKey: "",
      device: "metal",
    },
    dependencies,
  );

  assert.deepEqual(await model.embed([{ kind: "text", text: "value" }]), {
    vectors: [[1, 2, 3]],
    truncated: [],
  });
  assert.deepEqual(providers, ["webgpu", "cpu"]);
  assert.equal(artifactResolutions, 1);
  assert.equal(activeProvider, "cpu");
  assert.equal(gpuDisposals, 1);
  assert.match(writes.join(""), /inference failed.*falling back to CPU/);
  await model.dispose();
});

test("Transformers.js reports inputs truncated by the feature extraction pipeline", async () => {
  const tokenizer = createTokenizer((text) =>
    text.includes("overflow") ? 3 : 2,
  );
  const extractor = Object.assign(
    async () => ({
      dims: [2, 3],
      data: Float32Array.from([1, 0, 0, 0, 1, 0]),
    }),
    { tokenizer, async dispose() {} },
  );
  const dependencies = {
    resolveArtifacts: createArtifactResolver(),
    loadRuntime: async () => ({
      async pipeline() {
        return extractor;
      },
    }),
  };

  const model = new TransformersJsEmbeddingModel(
    entry({ maxInputTokens: 2 }),
    {
      apiKey: "",
    },
    dependencies,
  );
  const result = await model.embed([
    { kind: "text", text: "fits" },
    { kind: "text", text: "overflow" },
  ]);

  assert.deepEqual(result.truncated, [1]);
  assert.deepEqual(result.vectors, [
    [1, 0, 0],
    [0, 1, 0],
  ]);
  assert.equal(tokenizer.calls.length, 1);
  assert.deepEqual(tokenizer.calls[0].input, [
    "passage: fits",
    "passage: overflow",
  ]);
  await model.dispose();
});

test("Transformers.js does not treat tokenizer failures as GPU inference failures", async () => {
  const providers = [];
  const extractor = Object.assign(
    async () => ({ dims: [1, 3], data: new Float32Array(3) }),
    {
      tokenizer: Object.assign(
        async () => {
          throw new Error("tokenizer failed");
        },
        { model_max_length: 4096 },
      ),
      async dispose() {},
    },
  );
  const dependencies = {
    resolveArtifacts: createArtifactResolver(),
    loadRuntime: async () => ({
      async pipeline(_task, _repo, options) {
        providers.push(options.session_options?.executionProviders[0]);
        return extractor;
      },
    }),
  };

  const model = new TransformersJsEmbeddingModel(
    entry(),
    {
      apiKey: "",
      device: "metal",
    },
    dependencies,
  );
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    (error) =>
      error.message === "Transformers.js tokenization failed" &&
      error.cause?.message === "tokenizer failed",
  );
  assert.deepEqual(providers, ["webgpu"]);
  await model.dispose();
});
