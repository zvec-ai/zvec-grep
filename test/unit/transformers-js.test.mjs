import assert from "node:assert/strict";
import test from "node:test";
import {
  TransformersJsEmbeddingModel,
  setTransformersJsRuntimeForTesting,
} from "../../dist/engine/models/providers/transformers-js/embedding.js";

function entry(overrides = {}) {
  return {
    backend: "transformers-js",
    id: "local/test-transformer",
    provider: "local",
    model: "test-transformer",
    repo: "test/model-ONNX",
    revision: "0123456789abcdef",
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

function fakeTokenizer(tokenCount = () => 1) {
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

test("Transformers.js adapter fixes artifact recipe and formats query/document inputs", async (t) => {
  const loads = [];
  const calls = [];
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
      tokenizer: fakeTokenizer(),
      async dispose() {
        disposals++;
      },
    },
  );
  setTransformersJsRuntimeForTesting(async () => ({
    async pipeline(task, repo, options) {
      loads.push({ task, repo, options });
      return extractor;
    },
  }));
  t.after(() => setTransformersJsRuntimeForTesting(null));

  const model = new TransformersJsEmbeddingModel(entry(), {
    apiKey: "",
    modelCacheDir: "/tmp/model-cache",
  });
  assert.deepEqual(
    await model.embed(
      [
        { kind: "text", text: "find auth" },
        { kind: "text", text: "find parser" },
      ],
      { purpose: "query" },
    ),
    [
      Array.from(Float32Array.from([0.1, 0.2, 0.3])),
      Array.from(Float32Array.from([1.1, 1.2, 1.3])),
    ],
  );
  await model.embed([{ kind: "text", text: "implementation" }]);

  assert.deepEqual(loads, [
    {
      task: "feature-extraction",
      repo: "test/model-ONNX",
      options: {
        cache_dir: "/tmp/model-cache",
        revision: "0123456789abcdef",
        dtype: "q8",
      },
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

test("Transformers.js adapter validates the returned batch tensor", async (t) => {
  const extractor = Object.assign(
    async () => ({ dims: [1, 2], data: new Float32Array(2) }),
    { tokenizer: fakeTokenizer(), async dispose() {} },
  );
  setTransformersJsRuntimeForTesting(async () => ({
    async pipeline() {
      return extractor;
    },
  }));
  t.after(() => setTransformersJsRuntimeForTesting(null));

  const model = new TransformersJsEmbeddingModel(entry(), { apiKey: "" });
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    (error) =>
      error.message === "Transformers.js embedding failed" &&
      error.cause?.message === "Transformers.js returned an unexpected tensor",
  );
  await model.dispose();
});

test("Transformers.js adapter maps Metal to WebGPU", async (t) => {
  const loads = [];
  const extractor = Object.assign(
    async () => ({ dims: [1, 3], data: new Float32Array(3) }),
    { tokenizer: fakeTokenizer(), async dispose() {} },
  );
  setTransformersJsRuntimeForTesting(async () => ({
    async pipeline(task, repo, options) {
      loads.push({ task, repo, options });
      return extractor;
    },
  }));
  t.after(() => setTransformersJsRuntimeForTesting(null));

  const model = new TransformersJsEmbeddingModel(entry(), {
    apiKey: "",
    llamaGpu: "metal",
  });
  await model.embed([{ kind: "text", text: "value" }]);

  assert.deepEqual(loads[0].options.session_options, {
    executionProviders: ["webgpu"],
  });
  await model.dispose();
});

test("Transformers.js adapter falls back to CPU when GPU initialization fails", async (t) => {
  const providers = [];
  const extractor = Object.assign(
    async () => ({ dims: [1, 3], data: new Float32Array(3) }),
    { tokenizer: fakeTokenizer(), async dispose() {} },
  );
  setTransformersJsRuntimeForTesting(async () => ({
    async pipeline(_task, _repo, options) {
      const provider = options.session_options?.executionProviders[0];
      providers.push(provider);
      if (provider === "webgpu") {
        throw new Error("GPU unavailable");
      }
      return extractor;
    },
  }));
  t.after(() => setTransformersJsRuntimeForTesting(null));

  const writes = [];
  t.mock.method(process.stderr, "write", (message) => {
    writes.push(String(message));
    return true;
  });
  const model = new TransformersJsEmbeddingModel(entry(), {
    apiKey: "",
    llamaGpu: "metal",
  });
  await model.embed([{ kind: "text", text: "value" }]);

  assert.deepEqual(providers, ["webgpu", "cpu"]);
  assert.match(writes.join(""), /falling back to CPU/);
  await model.dispose();
});

test("Transformers.js adapter retries on CPU when GPU inference returns invalid values", async (t) => {
  const providers = [];
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
        tokenizer: fakeTokenizer(),
        async dispose() {
          if (provider === "webgpu") {
            gpuDisposals++;
          }
        },
      },
    );
  setTransformersJsRuntimeForTesting(async () => ({
    async pipeline(_task, _repo, options) {
      activeProvider = options.session_options?.executionProviders[0];
      providers.push(activeProvider);
      return makeExtractor(activeProvider);
    },
  }));
  t.after(() => setTransformersJsRuntimeForTesting(null));

  const writes = [];
  t.mock.method(process.stderr, "write", (message) => {
    writes.push(String(message));
    return true;
  });
  const model = new TransformersJsEmbeddingModel(entry(), {
    apiKey: "",
    llamaGpu: "metal",
  });

  assert.deepEqual(await model.embed([{ kind: "text", text: "value" }]), [
    [1, 2, 3],
  ]);
  assert.deepEqual(providers, ["webgpu", "cpu"]);
  assert.equal(activeProvider, "cpu");
  assert.equal(gpuDisposals, 1);
  assert.match(writes.join(""), /inference failed.*falling back to CPU/);
  await model.dispose();
});

test("Transformers.js reports inputs truncated by the feature extraction pipeline", async (t) => {
  const tokenizer = fakeTokenizer((text) =>
    text.includes("overflow") ? 3 : 2,
  );
  const extractor = Object.assign(
    async () => ({
      dims: [2, 3],
      data: Float32Array.from([1, 0, 0, 0, 1, 0]),
    }),
    { tokenizer, async dispose() {} },
  );
  setTransformersJsRuntimeForTesting(async () => ({
    async pipeline() {
      return extractor;
    },
  }));
  t.after(() => setTransformersJsRuntimeForTesting(null));

  const model = new TransformersJsEmbeddingModel(entry({ maxInputTokens: 2 }), {
    apiKey: "",
  });
  const result = await model.embedWithDiagnostics([
    { kind: "text", text: "fits" },
    { kind: "text", text: "overflow" },
  ]);

  assert.deepEqual(result.diagnostics.truncatedInputIndexes, [1]);
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

test("Transformers.js does not treat tokenizer failures as GPU inference failures", async (t) => {
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
  setTransformersJsRuntimeForTesting(async () => ({
    async pipeline(_task, _repo, options) {
      providers.push(options.session_options?.executionProviders[0]);
      return extractor;
    },
  }));
  t.after(() => setTransformersJsRuntimeForTesting(null));

  const model = new TransformersJsEmbeddingModel(entry(), {
    apiKey: "",
    llamaGpu: "metal",
  });
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    (error) =>
      error.message === "Transformers.js tokenization failed" &&
      error.cause?.message === "tokenizer failed",
  );
  assert.deepEqual(providers, ["webgpu"]);
  await model.dispose();
});
