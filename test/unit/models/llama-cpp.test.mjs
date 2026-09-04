import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import {
  LlamaCppEmbeddingModel,
  estimateLlamaEmbeddingContextVramMb,
  readGgufVocabSize,
  resolveLlamaGpuContextParallelism,
} from "../../../dist/engine/models/backends/llama-cpp.js";
import { createTemporaryDirectory } from "../../helpers/fixtures.mjs";

function entry(overrides = {}) {
  return {
    reference: "local/test-model",
    provider: "local",
    model: "test-model",
    uri: "hf:test/model.gguf",
    dimension: 2,
    metric: "cosine",
    format: "embeddinggemma",
    contextSize: 8,
    maxBatchSize: 8,
    ...overrides,
  };
}

async function ggufFile(t, name = "model.gguf", contents = "GGUFpayload") {
  const root = await createTemporaryDirectory(t, "zvec-llama-");
  const path = join(root, name);
  await mkdir(root, { recursive: true });
  await writeFile(path, contents);
  return { root, path };
}

function createDependencies(modelPath, options = {}) {
  const calls = {
    llama: [],
    model: [],
    contexts: [],
    texts: [],
    disposedContexts: 0,
    disposedModels: 0,
    disposedLlamas: 0,
    lifecycle: [],
  };
  const model = {
    trainContextSize: options.trainContextSize ?? 6,
    tokenize: (text) => [...text],
    detokenize: (tokens) => tokens.join(""),
    createEmbeddingContext: async (contextOptions) => {
      calls.contexts.push(contextOptions);
      if (
        options.failContextAfter !== undefined &&
        calls.contexts.length > options.failContextAfter
      ) {
        throw new Error("context failed");
      }
      return {
        getEmbeddingFor: async (text) => {
          calls.texts.push(text);
          if (options.failEmbedding) throw new Error("embedding failed");
          return { vector: [text.length, 1] };
        },
        dispose: async () => {
          calls.disposedContexts++;
        },
      };
    },
    dispose: async () => {
      calls.disposedModels++;
    },
  };
  let loadModelAttempts = 0;
  const makeLlama = (gpu) => ({
    gpu,
    cpuMathCores: 8,
    supportsGpuOffloading: true,
    getVramState: options.vramError
      ? async () => {
          throw new Error("vram unavailable");
        }
      : async () => options.vram ?? { total: 8e9, used: 2e9, free: 6e9 },
    loadModel: async (loadOptions) => {
      calls.model.push(loadOptions);
      loadModelAttempts++;
      if (options.failFirstModel && loadModelAttempts === 1) {
        throw new Error("model GPU failure");
      }
      return model;
    },
    dispose: async () => {
      calls.disposedLlamas++;
    },
  });
  const runtime = {
    LlamaLogLevel: { error: "error" },
    resolveModelFile: async (_uri, resolveOptions) => {
      calls.lifecycle.push("resolveModelFile");
      calls.resolveOptions = resolveOptions;
      resolveOptions.onProgress?.({
        downloadedSize: 25,
        totalSize: 100,
      });
      return modelPath;
    },
    getLlama: async (llamaOptions) => {
      calls.lifecycle.push("getLlama");
      calls.llama.push(llamaOptions);
      if (options.failGpu && llamaOptions.gpu !== false) {
        throw new Error("GPU unavailable");
      }
      if (options.failCpu && llamaOptions.gpu === false) {
        throw new Error("CPU backend unavailable");
      }
      return makeLlama(llamaOptions.gpu);
    },
  };
  return {
    dependencies: {
      loadRuntime: async () => runtime,
      runtimeState: {
        failedGpuInitModes: new Set(),
        cpuCompatibleFallbackWarningShown: false,
      },
    },
    calls,
  };
}

async function captureStderr(callback) {
  const messages = [];
  const original = process.stderr.write;
  process.stderr.write = (value) => {
    messages.push(String(value));
    return true;
  };
  try {
    return { result: await callback(), messages };
  } finally {
    process.stderr.write = original;
  }
}

const GGUF_TYPE = {
  UINT8: 0,
  INT8: 1,
  UINT16: 2,
  INT16: 3,
  UINT32: 4,
  INT32: 5,
  FLOAT32: 6,
  BOOL: 7,
  STRING: 8,
  ARRAY: 9,
  UINT64: 10,
  INT64: 11,
  FLOAT64: 12,
};

function u32(value) {
  const buffer = Buffer.alloc(4);
  buffer.writeUInt32LE(value);
  return buffer;
}

function i32(value) {
  const buffer = Buffer.alloc(4);
  buffer.writeInt32LE(value);
  return buffer;
}

function u64(value) {
  const buffer = Buffer.alloc(8);
  buffer.writeBigUInt64LE(BigInt(value));
  return buffer;
}

function i64(value) {
  const buffer = Buffer.alloc(8);
  buffer.writeBigInt64LE(BigInt(value));
  return buffer;
}

function ggufString(value) {
  const bytes = Buffer.from(value, "utf8");
  return Buffer.concat([u64(bytes.length), bytes]);
}

function encodeGgufValue(type, value) {
  switch (type) {
    case GGUF_TYPE.UINT8:
      return Buffer.from([value]);
    case GGUF_TYPE.INT8:
      return Buffer.from([value]);
    case GGUF_TYPE.UINT16: {
      const buffer = Buffer.alloc(2);
      buffer.writeUInt16LE(value);
      return buffer;
    }
    case GGUF_TYPE.INT16: {
      const buffer = Buffer.alloc(2);
      buffer.writeInt16LE(value);
      return buffer;
    }
    case GGUF_TYPE.UINT32:
      return u32(value);
    case GGUF_TYPE.INT32:
      return i32(value);
    case GGUF_TYPE.FLOAT32: {
      const buffer = Buffer.alloc(4);
      buffer.writeFloatLE(value);
      return buffer;
    }
    case GGUF_TYPE.BOOL:
      return Buffer.from([value ? 1 : 0]);
    case GGUF_TYPE.STRING:
      return ggufString(value);
    case GGUF_TYPE.UINT64:
      return u64(value);
    case GGUF_TYPE.INT64:
      return i64(value);
    case GGUF_TYPE.FLOAT64: {
      const buffer = Buffer.alloc(8);
      buffer.writeDoubleLE(value);
      return buffer;
    }
    case GGUF_TYPE.ARRAY: {
      const parts = [u32(value.elementType), u64(value.items.length)];
      for (const item of value.items) {
        parts.push(encodeGgufValue(value.elementType, item));
      }
      return Buffer.concat(parts);
    }
    default:
      throw new Error(`unsupported test GGUF type ${type}`);
  }
}

function encodeGguf(entries, options = {}) {
  const parts = [
    Buffer.from("GGUF"),
    u32(options.version ?? 3),
    u64(options.tensorCount ?? 0),
    u64(options.keyCount ?? entries.length),
  ];
  for (const [key, type, value] of entries) {
    parts.push(ggufString(key), u32(type));
    parts.push(Buffer.isBuffer(value) ? value : encodeGgufValue(type, value));
  }
  if (options.trailer) {
    parts.push(options.trailer);
  }
  return Buffer.concat(parts);
}

function withParallelismEnv(t, value) {
  const previous = process.env.ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM;
  if (value === undefined) {
    delete process.env.ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM;
  } else {
    process.env.ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM = value;
  }
  t.after(() => {
    if (previous === undefined) {
      delete process.env.ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM;
    } else {
      process.env.ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM = previous;
    }
  });
}

test("local embedding loads GGUF, formats and truncates text, parallelizes, caches, and disposes", async (t) => {
  const modelFile = await ggufFile(t);
  const setup = createDependencies(modelFile.path);
  const previousParallelism = process.env.ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM;
  process.env.ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM = "2";
  t.after(() => {
    if (previousParallelism === undefined) {
      delete process.env.ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM;
    } else {
      process.env.ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM = previousParallelism;
    }
  });

  const model = new LlamaCppEmbeddingModel(
    entry(),
    {
      modelCacheDir: modelFile.root,
      device: "cpu",
    },
    setup.dependencies,
  );
  const downloadProgress = [];
  const result = await model.embed(
    [
      { kind: "text", text: "abcdefghijk" },
      { kind: "text", text: "second" },
      { kind: "text", text: "third" },
    ],
    {
      purpose: "query",
      onProgress: (progress) => downloadProgress.push(progress),
    },
  );
  const vectors = result.vectors;
  assert.equal(vectors.length, 3);
  assert.deepEqual(result.truncated, [0, 1, 2]);
  assert.equal(setup.calls.contexts.length, 2);
  assert.equal(setup.calls.contexts[0].threads, 4);
  assert.equal(
    setup.calls.texts.every((text) => text.length <= 2),
    true,
  );
  assert.equal(setup.calls.model[0].gpuLayers, 0);
  assert.equal(setup.calls.resolveOptions.cli, false);
  assert.deepEqual(setup.calls.lifecycle.slice(0, 2), [
    "resolveModelFile",
    "getLlama",
  ]);
  assert.deepEqual(downloadProgress, [
    {
      stage: "preparing",
      model: "local/test-model",
    },
    {
      stage: "downloading",
      model: "local/test-model",
      downloadedBytes: 25,
      totalBytes: 100,
    },
    {
      stage: "ready",
      model: "local/test-model",
    },
  ]);

  await model.embed([{ kind: "text", text: "cached" }]);
  assert.equal(setup.calls.model.length, 1);
  await model.dispose();
  await model.dispose();
  assert.equal(setup.calls.disposedContexts, 2);
  assert.equal(setup.calls.disposedModels, 1);
  assert.equal(setup.calls.disposedLlamas, 1);
  await assert.rejects(
    model.embed([{ kind: "text", text: "after dispose" }]),
    /model is disposed/,
  );
});

test("local embedding supports qwen query format, automatic GPU parallelism, and context partial capacity", async (t) => {
  withParallelismEnv(t, undefined);
  const modelFile = await ggufFile(t);
  const setup = createDependencies(modelFile.path, {
    failContextAfter: 2,
    trainContextSize: 200,
  });
  const model = new LlamaCppEmbeddingModel(
    entry({ format: "qwen3", contextSize: 100 }),
    { modelCacheDir: modelFile.root, device: "metal" },
    setup.dependencies,
  );
  const { vectors } = await model.embed(
    Array.from({ length: 4 }, (_, index) => ({
      kind: "text",
      text: `query-${index}`,
    })),
    { purpose: "query" },
  );
  assert.equal(vectors.length, 4);
  assert.equal(setup.calls.contexts.length, 3);
  assert.match(setup.calls.texts[0], /^Instruct:/);
  assert.equal(setup.calls.contexts[0].threads, 0);
  assert.equal("gpuLayers" in setup.calls.model[0], false);
  await model.dispose();
});

test("local embedding falls back from GPU initialization to CPU", async (t) => {
  const modelFile = await ggufFile(t);
  const setup = createDependencies(modelFile.path, { failGpu: true });
  const output = await captureStderr(async () => {
    const model = new LlamaCppEmbeddingModel(
      entry(),
      {
        modelCacheDir: modelFile.root,
        device: "metal",
      },
      setup.dependencies,
    );
    await model.embed([{ kind: "text", text: "value" }]);
    await model.dispose();
  });
  assert.match(output.messages.join(""), /GPU init failed/);
  assert.deepEqual(
    setup.calls.llama.map((item) => item.gpu),
    ["metal", false],
  );
});

test("local embedding falls back to packaged backend and retries model/context GPU failures", async (t) => {
  const modelFile = await ggufFile(t);
  const cpuFallback = createDependencies(modelFile.path, { failCpu: true });
  const cpuOutput = await captureStderr(async () => {
    const model = new LlamaCppEmbeddingModel(
      entry(),
      {
        modelCacheDir: modelFile.root,
        device: "cpu",
      },
      cpuFallback.dependencies,
    );
    await model.embed([{ kind: "text", text: "value" }]);
    await model.dispose();
  });
  assert.match(
    cpuOutput.messages.join(""),
    /CPU-only llama.cpp backend unavailable/,
  );
  assert.deepEqual(
    cpuFallback.calls.llama.map((item) => item.gpu),
    [false, "auto"],
  );

  const modelRetry = createDependencies(modelFile.path, {
    failFirstModel: true,
  });
  const retryOutput = await captureStderr(async () => {
    const model = new LlamaCppEmbeddingModel(
      entry(),
      {
        modelCacheDir: modelFile.root,
        device: "auto",
      },
      modelRetry.dependencies,
    );
    await model.embed([{ kind: "text", text: "value" }]);
    await model.dispose();
  });
  assert.match(retryOutput.messages.join(""), /GPU model load failed/);
  assert.equal(modelRetry.calls.model.length, 2);
});

test("local embedding rejects invalid downloaded GGUF and removes corrupt artifacts", async (t) => {
  for (const [contents, message] of [
    ["<!doctype html><html>failure</html>", /HTML, not GGUF/],
    ["NOPE invalid binary", /not a valid GGUF/],
  ]) {
    const modelFile = await ggufFile(t, `bad-${Math.random()}.gguf`, contents);
    const setup = createDependencies(modelFile.path);
    const model = new LlamaCppEmbeddingModel(
      entry(),
      {
        modelCacheDir: modelFile.root,
        device: "cpu",
      },
      setup.dependencies,
    );
    await assert.rejects(
      model.embed([{ kind: "text", text: "value" }]),
      (error) =>
        /embedding failed/.test(error.message) &&
        message.test(String(error.cause?.message)),
    );
  }
});

test("local embedding reports context and embedding runtime failures", async (t) => {
  const modelFile = await ggufFile(t);
  const setup = createDependencies(modelFile.path, {
    failContextAfter: 0,
    failEmbedding: true,
  });
  const model = new LlamaCppEmbeddingModel(
    entry(),
    {
      modelCacheDir: modelFile.root,
      device: "cpu",
    },
    setup.dependencies,
  );
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    /embedding failed/,
  );
});

const QWEN3_VOCAB_SIZE = 151_936;
const QWEN3_CONTEXT_SIZE = 8_192;
const RTX_4090_FREE_VRAM_BYTES = 21 * 1024 * 1024 * 1024;

test("llama embedding VRAM estimate includes large-vocab logits buffers", () => {
  assert.equal(estimateLlamaEmbeddingContextVramMb({ contextSize: 2048 }), 150);
  assert.equal(
    estimateLlamaEmbeddingContextVramMb({
      contextSize: 8192,
      vocabSize: 0,
    }),
    150,
  );

  const qwenEstimate = estimateLlamaEmbeddingContextVramMb({
    contextSize: QWEN3_CONTEXT_SIZE,
    vocabSize: QWEN3_VOCAB_SIZE,
  });
  const logitsMb = (QWEN3_CONTEXT_SIZE * QWEN3_VOCAB_SIZE * 4) / (1024 * 1024);
  assert.equal(qwenEstimate, 150 + logitsMb);
  assert.ok(qwenEstimate > 4700);

  assert.equal(
    resolveLlamaGpuContextParallelism({
      freeVramBytes: RTX_4090_FREE_VRAM_BYTES,
      contextSize: 2048,
    }),
    8,
  );
  assert.equal(
    resolveLlamaGpuContextParallelism({
      freeVramBytes: RTX_4090_FREE_VRAM_BYTES,
      contextSize: QWEN3_CONTEXT_SIZE,
      vocabSize: QWEN3_VOCAB_SIZE,
    }),
    1,
  );
  assert.equal(
    resolveLlamaGpuContextParallelism({
      freeVramBytes: RTX_4090_FREE_VRAM_BYTES,
      contextSize: QWEN3_CONTEXT_SIZE,
      vocabSize: QWEN3_VOCAB_SIZE,
      cap: 4,
    }),
    1,
  );
});

test("readGgufVocabSize parses vocab_size and tokenizer token counts", async (t) => {
  const missing = await ggufFile(t, "missing.gguf");
  assert.equal(readGgufVocabSize(`${missing.path}-absent`), undefined);
  assert.equal(readGgufVocabSize(missing.path), undefined);

  const withMetadata = await ggufFile(
    t,
    "qwen3.gguf",
    encodeGguf([
      ["general.architecture", GGUF_TYPE.STRING, "qwen3"],
      ["general.name", GGUF_TYPE.STRING, "fixture"],
      ["qwen3.block_count", GGUF_TYPE.UINT32, 28],
      ["qwen3.attention.causal", GGUF_TYPE.BOOL, 1],
      ["qwen3.rope.freq_base", GGUF_TYPE.FLOAT32, 1_000_000],
      ["qwen3.logit_scale", GGUF_TYPE.FLOAT64, 1],
      [
        "tokenizer.ggml.scores",
        GGUF_TYPE.ARRAY,
        { elementType: GGUF_TYPE.FLOAT32, items: [0.1, 0.2] },
      ],
      [
        "tokenizer.ggml.merges",
        GGUF_TYPE.ARRAY,
        { elementType: GGUF_TYPE.STRING, items: ["a b", "c d"] },
      ],
      [
        "nested.values",
        GGUF_TYPE.ARRAY,
        {
          elementType: GGUF_TYPE.ARRAY,
          items: [{ elementType: GGUF_TYPE.UINT32, items: [1, 2] }],
        },
      ],
      ["qwen3.vocab_size", GGUF_TYPE.UINT32, QWEN3_VOCAB_SIZE],
    ]),
  );
  assert.equal(readGgufVocabSize(withMetadata.path), QWEN3_VOCAB_SIZE);

  const uint64Vocab = await ggufFile(
    t,
    "uint64-vocab.gguf",
    encodeGguf([["llama.vocab_size", GGUF_TYPE.UINT64, 32_000]]),
  );
  assert.equal(readGgufVocabSize(uint64Vocab.path), 32_000);

  const int32Vocab = await ggufFile(
    t,
    "int32-vocab.gguf",
    encodeGguf([["gemma.vocab_size", GGUF_TYPE.INT32, 256_000]]),
  );
  assert.equal(readGgufVocabSize(int32Vocab.path), 256_000);

  const int64Vocab = await ggufFile(
    t,
    "int64-vocab.gguf",
    encodeGguf([["bert.vocab_size", GGUF_TYPE.INT64, 30_522]]),
  );
  assert.equal(readGgufVocabSize(int64Vocab.path), 30_522);

  const uint8Vocab = await ggufFile(
    t,
    "uint8-vocab.gguf",
    encodeGguf([["tiny.vocab_size", GGUF_TYPE.UINT8, 16]]),
  );
  assert.equal(readGgufVocabSize(uint8Vocab.path), 16);

  const int8Vocab = await ggufFile(
    t,
    "int8-vocab.gguf",
    encodeGguf([["tiny.vocab_size", GGUF_TYPE.INT8, 12]]),
  );
  assert.equal(readGgufVocabSize(int8Vocab.path), 12);

  const uint16Vocab = await ggufFile(
    t,
    "uint16-vocab.gguf",
    encodeGguf([["tiny.vocab_size", GGUF_TYPE.UINT16, 512]]),
  );
  assert.equal(readGgufVocabSize(uint16Vocab.path), 512);

  const int16Vocab = await ggufFile(
    t,
    "int16-vocab.gguf",
    encodeGguf([["tiny.vocab_size", GGUF_TYPE.INT16, 256]]),
  );
  assert.equal(readGgufVocabSize(int16Vocab.path), 256);

  const tokenFallback = await ggufFile(
    t,
    "tokens.gguf",
    encodeGguf([
      [
        "tokenizer.ggml.tokens",
        GGUF_TYPE.ARRAY,
        { elementType: GGUF_TYPE.STRING, items: ["<pad>", "hello", "world"] },
      ],
    ]),
  );
  assert.equal(readGgufVocabSize(tokenFallback.path), 3);

  const skippedNonNumeric = await ggufFile(
    t,
    "skip-float.gguf",
    encodeGguf([
      ["qwen3.vocab_size", GGUF_TYPE.FLOAT32, 12.5],
      ["qwen3.vocab_size", GGUF_TYPE.UINT32, 151_936],
    ]),
  );
  assert.equal(readGgufVocabSize(skippedNonNumeric.path), 151_936);

  const truncated = await ggufFile(t, "truncated.gguf", Buffer.from("GGUF"));
  assert.equal(readGgufVocabSize(truncated.path), undefined);

  const unsupportedVersion = await ggufFile(
    t,
    "v1.gguf",
    encodeGguf([["qwen3.vocab_size", GGUF_TYPE.UINT32, 100]], { version: 1 }),
  );
  assert.equal(readGgufVocabSize(unsupportedVersion.path), undefined);

  const tooManyKeys = await ggufFile(
    t,
    "too-many-keys.gguf",
    encodeGguf([], { keyCount: 10_001 }),
  );
  assert.equal(readGgufVocabSize(tooManyKeys.path), undefined);

  const unknownType = await ggufFile(
    t,
    "unknown-type.gguf",
    encodeGguf([["qwen3.hidden", 99, u32(1)]]),
  );
  assert.equal(readGgufVocabSize(unknownType.path), undefined);

  const emptyTokens = await ggufFile(
    t,
    "empty-tokens.gguf",
    encodeGguf([
      [
        "tokenizer.ggml.tokens",
        GGUF_TYPE.ARRAY,
        { elementType: GGUF_TYPE.STRING, items: [] },
      ],
      ["qwen3.vocab_size", GGUF_TYPE.UINT32, 128],
    ]),
  );
  assert.equal(readGgufVocabSize(emptyTokens.path), 128);
});

test("local embedding caps GPU parallelism for large-vocab GGUF models", async (t) => {
  withParallelismEnv(t, undefined);
  const modelFile = await ggufFile(
    t,
    "qwen3-embedding.gguf",
    encodeGguf([
      ["general.architecture", GGUF_TYPE.STRING, "qwen3"],
      ["qwen3.vocab_size", GGUF_TYPE.UINT32, QWEN3_VOCAB_SIZE],
    ]),
  );
  const setup = createDependencies(modelFile.path, {
    vram: {
      total: 24 * 1024 * 1024 * 1024,
      used: 3 * 1024 * 1024 * 1024,
      free: RTX_4090_FREE_VRAM_BYTES,
    },
  });
  const model = new LlamaCppEmbeddingModel(
    entry({ format: "qwen3", contextSize: QWEN3_CONTEXT_SIZE }),
    { modelCacheDir: modelFile.root, device: "cuda" },
    setup.dependencies,
  );
  const { vectors } = await model.embed(
    Array.from({ length: 8 }, (_, index) => ({
      kind: "text",
      text: `chunk-${index}`,
    })),
  );
  assert.equal(vectors.length, 8);
  assert.equal(setup.calls.contexts.length, 1);
  await model.dispose();
});

test("local embedding keeps ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM override for large-vocab models", async (t) => {
  withParallelismEnv(t, "3");
  const modelFile = await ggufFile(
    t,
    "qwen3-override.gguf",
    encodeGguf([["qwen3.vocab_size", GGUF_TYPE.UINT32, QWEN3_VOCAB_SIZE]]),
  );
  const setup = createDependencies(modelFile.path, {
    vram: {
      total: 24 * 1024 * 1024 * 1024,
      used: 3 * 1024 * 1024 * 1024,
      free: RTX_4090_FREE_VRAM_BYTES,
    },
  });
  const model = new LlamaCppEmbeddingModel(
    entry({ format: "qwen3", contextSize: QWEN3_CONTEXT_SIZE }),
    { modelCacheDir: modelFile.root, device: "cuda" },
    setup.dependencies,
  );
  await model.embed(
    Array.from({ length: 6 }, (_, index) => ({
      kind: "text",
      text: `chunk-${index}`,
    })),
  );
  assert.equal(setup.calls.contexts.length, 3);
  await model.dispose();
});

test("local embedding falls back to conservative GPU parallelism when VRAM query fails", async (t) => {
  withParallelismEnv(t, undefined);
  const modelFile = await ggufFile(t);
  const setup = createDependencies(modelFile.path, { vramError: true });
  const model = new LlamaCppEmbeddingModel(
    entry(),
    { modelCacheDir: modelFile.root, device: "cuda" },
    setup.dependencies,
  );
  await model.embed(
    Array.from({ length: 4 }, (_, index) => ({
      kind: "text",
      text: `chunk-${index}`,
    })),
  );
  assert.equal(setup.calls.contexts.length, 2);
  await model.dispose();
});
