import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import {
  LlamaCppEmbeddingModel,
  setLlamaCppRuntimeForTesting,
} from "../../dist/engine/models/providers/llama-cpp/embedding.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

function entry(overrides = {}) {
  return {
    id: "local/test-model",
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

function fakeRuntime(modelPath, options = {}) {
  const calls = {
    llama: [],
    model: [],
    contexts: [],
    texts: [],
    disposedContexts: 0,
    disposedModels: 0,
    disposedLlamas: 0,
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
      : async () => ({ total: 8e9, used: 2e9, free: 6e9 }),
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
      calls.resolveOptions = resolveOptions;
      return modelPath;
    },
    getLlama: async (llamaOptions) => {
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
  return { runtime, calls };
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

test("local embedding loads GGUF, formats and truncates text, parallelizes, caches, and disposes", async (t) => {
  const modelFile = await ggufFile(t);
  const fake = fakeRuntime(modelFile.path);
  setLlamaCppRuntimeForTesting(async () => fake.runtime);
  t.after(() => setLlamaCppRuntimeForTesting(null));

  const model = new LlamaCppEmbeddingModel(entry(), {
    modelCacheDir: modelFile.root,
    llamaGpu: false,
    embeddingParallelism: 2,
  });
  const vectors = await model.embed(
    [
      { kind: "text", text: "abcdefghijk" },
      { kind: "text", text: "second" },
      { kind: "text", text: "third" },
    ],
    { purpose: "query" },
  );
  assert.equal(vectors.length, 3);
  assert.equal(fake.calls.contexts.length, 2);
  assert.equal(fake.calls.contexts[0].threads, 4);
  assert.equal(
    fake.calls.texts.every((text) => text.length <= 2),
    true,
  );
  assert.equal(fake.calls.model[0].gpuLayers, 0);
  assert.equal(fake.calls.resolveOptions.cli, false);

  await model.embed([{ kind: "text", text: "cached" }]);
  assert.equal(fake.calls.model.length, 1);
  await model.dispose();
  await model.dispose();
  assert.equal(fake.calls.disposedContexts, 2);
  assert.equal(fake.calls.disposedModels, 1);
  assert.equal(fake.calls.disposedLlamas, 1);
  await assert.rejects(
    model.embed([{ kind: "text", text: "after dispose" }]),
    /model is disposed/,
  );
});

test("local embedding supports qwen query format, automatic GPU parallelism, and context partial capacity", async (t) => {
  const modelFile = await ggufFile(t);
  const fake = fakeRuntime(modelFile.path, {
    failContextAfter: 2,
    trainContextSize: 200,
  });
  setLlamaCppRuntimeForTesting(async () => fake.runtime);
  t.after(() => setLlamaCppRuntimeForTesting(null));
  const model = new LlamaCppEmbeddingModel(
    entry({ format: "qwen3", contextSize: 100 }),
    { modelCacheDir: modelFile.root, llamaGpu: "metal" },
  );
  const vectors = await model.embed(
    Array.from({ length: 4 }, (_, index) => ({
      kind: "text",
      text: `query-${index}`,
    })),
    { purpose: "query" },
  );
  assert.equal(vectors.length, 4);
  assert.equal(fake.calls.contexts.length, 3);
  assert.match(fake.calls.texts[0], /^Instruct:/);
  assert.equal(fake.calls.contexts[0].threads, 0);
  assert.equal("gpuLayers" in fake.calls.model[0], false);
  await model.dispose();
});

test("local embedding falls back from GPU initialization and cached failed modes to CPU", async (t) => {
  const modelFile = await ggufFile(t);
  const first = fakeRuntime(modelFile.path, { failGpu: true });
  setLlamaCppRuntimeForTesting(async () => first.runtime);
  t.after(() => setLlamaCppRuntimeForTesting(null));
  const output = await captureStderr(async () => {
    const model = new LlamaCppEmbeddingModel(entry(), {
      modelCacheDir: modelFile.root,
      llamaGpu: "metal",
    });
    await model.embed([{ kind: "text", text: "value" }]);
    await model.dispose();
  });
  assert.match(output.messages.join(""), /GPU init failed/);
  assert.deepEqual(
    first.calls.llama.map((item) => item.gpu),
    ["metal", false],
  );

  const second = fakeRuntime(modelFile.path);
  setLlamaCppRuntimeForTesting(async () => second.runtime);
  const model = new LlamaCppEmbeddingModel(entry(), {
    modelCacheDir: modelFile.root,
    llamaGpu: false,
  });
  await model.embed([{ kind: "text", text: "value" }]);
  await model.dispose();
});

test("local embedding falls back to packaged backend and retries model/context GPU failures", async (t) => {
  const modelFile = await ggufFile(t);
  const cpuFallback = fakeRuntime(modelFile.path, { failCpu: true });
  setLlamaCppRuntimeForTesting(async () => cpuFallback.runtime);
  t.after(() => setLlamaCppRuntimeForTesting(null));
  const cpuOutput = await captureStderr(async () => {
    const model = new LlamaCppEmbeddingModel(entry(), {
      modelCacheDir: modelFile.root,
      llamaGpu: false,
    });
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

  const modelRetry = fakeRuntime(modelFile.path, { failFirstModel: true });
  setLlamaCppRuntimeForTesting(async () => modelRetry.runtime);
  const retryOutput = await captureStderr(async () => {
    const model = new LlamaCppEmbeddingModel(entry(), {
      modelCacheDir: modelFile.root,
      llamaGpu: "auto",
    });
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
    const fake = fakeRuntime(modelFile.path);
    setLlamaCppRuntimeForTesting(async () => fake.runtime);
    const model = new LlamaCppEmbeddingModel(entry(), {
      modelCacheDir: modelFile.root,
      llamaGpu: false,
    });
    await assert.rejects(
      model.embed([{ kind: "text", text: "value" }]),
      (error) =>
        /embedding failed/.test(error.message) &&
        message.test(String(error.cause?.message)),
    );
  }
  setLlamaCppRuntimeForTesting(null);
});

test("local embedding reports context and embedding runtime failures", async (t) => {
  const modelFile = await ggufFile(t);
  const failed = fakeRuntime(modelFile.path, {
    failContextAfter: 0,
    failEmbedding: true,
  });
  setLlamaCppRuntimeForTesting(async () => failed.runtime);
  t.after(() => setLlamaCppRuntimeForTesting(null));
  const model = new LlamaCppEmbeddingModel(entry(), {
    modelCacheDir: modelFile.root,
    llamaGpu: false,
  });
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    /embedding failed/,
  );
});
