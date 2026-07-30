import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { LlamaCppEmbeddingModel } from "../../../dist/engine/models/backends/llama-cpp.js";
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
  const result = await model.embed(
    [
      { kind: "text", text: "abcdefghijk" },
      { kind: "text", text: "second" },
      { kind: "text", text: "third" },
    ],
    { purpose: "query" },
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
