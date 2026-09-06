import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import test from "node:test";
import { LlamaCppEmbeddingModel } from "../../../dist/engine/models/backends/llama-cpp.js";
import { createTemporaryDirectory } from "../../helpers/fixtures.mjs";

function entry(overrides = {}) {
  return {
    reference: "local/test-model",
    provider: "local",
    model: "test-model",
    uri: "hf:test/model/model.gguf#hf-revision",
    cacheFile: "hf_test_model.gguf",
    sources: {
      huggingFace: {
        repo: "test/model",
        revision: "hf-revision",
      },
      modelScope: {
        repo: "mirror/test-model",
        revision: "ms-revision",
      },
    },
    artifacts: [
      {
        path: "model.gguf",
        size: 100,
        sha256: "a".repeat(64),
      },
    ],
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
    runtimeLoads: 0,
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
      loadRuntime: async () => {
        calls.runtimeLoads++;
        if (options.failRuntimeLoad) {
          throw new Error("runtime import failed");
        }
        return runtime;
      },
      resolveArtifacts: async (resolveOptions) => {
        calls.lifecycle.push("resolveArtifacts");
        calls.resolveOptions = resolveOptions;
        if (options.failArtifactResolution) {
          throw new Error("artifact download failed");
        }
        resolveOptions.onDownloadPlan?.(resolveOptions.artifacts);
        resolveOptions.onProgress?.({
          model: resolveOptions.model,
          source: "huggingface",
          artifact: "model.gguf",
          downloadedBytes: 25,
          totalBytes: 100,
        });
        if (options.useModelScope) {
          resolveOptions.onFallback?.(
            "Hugging Face unavailable; using ModelScope.",
          );
          if (options.duplicateFallbackWarning) {
            resolveOptions.onFallback?.("duplicate fallback warning");
          }
          resolveOptions.onDownloadPlan?.(resolveOptions.artifacts);
          resolveOptions.onProgress?.({
            model: resolveOptions.model,
            source: "modelscope",
            artifact: "model.gguf",
            downloadedBytes: 0,
            totalBytes: 100,
          });
        }
        const source = resolveOptions.sources[options.useModelScope ? 1 : 0];
        return {
          source,
          directory: source.cacheDirectory,
          paths: { "model.gguf": modelPath },
        };
      },
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
  assert.equal(setup.calls.model[0].modelPath, modelFile.path);
  const { onDownloadPlan, onProgress, onFallback, ...resolution } =
    setup.calls.resolveOptions;
  assert.equal(typeof onDownloadPlan, "function");
  assert.equal(typeof onProgress, "function");
  assert.equal(typeof onFallback, "function");
  assert.deepEqual(resolution, {
    model: "local/test-model",
    sources: [
      {
        kind: "huggingface",
        repo: "test/model",
        revision: "hf-revision",
        cacheDirectory: modelFile.root,
        localPaths: { "model.gguf": "hf_test_model.gguf" },
      },
      {
        kind: "modelscope",
        repo: "mirror/test-model",
        revision: "ms-revision",
        cacheDirectory: join(
          modelFile.root,
          "modelscope",
          "llama-cpp",
          "mirror--test-model",
          "ms-revision",
        ),
      },
    ],
    artifacts: entry().artifacts,
  });
  assert.deepEqual(setup.calls.lifecycle.slice(0, 2), [
    "resolveArtifacts",
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
  assert.equal(
    setup.calls.lifecycle.filter((event) => event === "resolveArtifacts")
      .length,
    1,
  );
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

test("local embedding uses the resolved ModelScope path and reports source fallback once", async (t) => {
  const cacheDirectory = await createTemporaryDirectory(t, "zvec-llama-ms-");
  const modelPath = join(
    cacheDirectory,
    "modelscope",
    "llama-cpp",
    "mirror--test-model",
    "ms-revision",
    "model.gguf",
  );
  await mkdir(dirname(modelPath), { recursive: true });
  await writeFile(modelPath, "GGUFpayload");
  const setup = createDependencies(modelPath, {
    useModelScope: true,
    duplicateFallbackWarning: true,
  });
  const progress = [];
  const model = new LlamaCppEmbeddingModel(
    entry(),
    { modelCacheDir: cacheDirectory, device: "cpu" },
    setup.dependencies,
  );

  await model.embed([{ kind: "text", text: "value" }], {
    onProgress: (event) => progress.push(event),
  });

  assert.equal(setup.calls.model[0].modelPath, modelPath);
  assert.equal(
    setup.calls.resolveOptions.sources[1].cacheDirectory,
    join(
      cacheDirectory,
      "modelscope",
      "llama-cpp",
      "mirror--test-model",
      "ms-revision",
    ),
  );
  assert.deepEqual(
    progress.filter((event) => event.stage === "downloading"),
    [
      {
        stage: "downloading",
        model: "local/test-model",
        downloadedBytes: 25,
        totalBytes: 100,
      },
      {
        stage: "downloading",
        model: "local/test-model",
        downloadedBytes: 0,
        totalBytes: 100,
      },
    ],
  );
  assert.deepEqual(
    progress.filter((event) => event.stage === "warning"),
    [
      {
        stage: "warning",
        model: "local/test-model",
        message: "Hugging Face unavailable; using ModelScope.",
      },
    ],
  );
  await model.dispose();
});

test("artifact resolution failures do not enter llama.cpp GPU fallback", async (t) => {
  const modelFile = await ggufFile(t);
  const setup = createDependencies(modelFile.path, {
    failArtifactResolution: true,
  });
  const model = new LlamaCppEmbeddingModel(
    entry(),
    { modelCacheDir: modelFile.root, device: "metal" },
    setup.dependencies,
  );

  const output = await captureStderr(async () => {
    await assert.rejects(
      model.embed([{ kind: "text", text: "value" }]),
      (error) =>
        error.message === "llama.cpp embedding failed" &&
        error.cause?.message === "artifact download failed",
    );
  });

  assert.equal(setup.calls.llama.length, 0);
  assert.equal(setup.calls.model.length, 0);
  assert.doesNotMatch(output.messages.join(""), /GPU|falling back to CPU/);
  await model.dispose();
});

test("runtime import failures do not enter llama.cpp GPU model fallback", async (t) => {
  const modelFile = await ggufFile(t);
  const setup = createDependencies(modelFile.path, { failRuntimeLoad: true });
  const model = new LlamaCppEmbeddingModel(
    entry(),
    { modelCacheDir: modelFile.root, device: "metal" },
    setup.dependencies,
  );

  const output = await captureStderr(async () => {
    await assert.rejects(
      model.embed([{ kind: "text", text: "value" }]),
      (error) =>
        error.message === "llama.cpp embedding failed" &&
        error.cause?.message === "runtime import failed",
    );
  });

  assert.equal(setup.calls.runtimeLoads, 1);
  assert.equal(setup.calls.llama.length, 0);
  assert.equal(setup.calls.model.length, 0);
  assert.doesNotMatch(output.messages.join(""), /GPU model load failed/);
  await model.dispose();
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
  assert.equal(setup.calls.lifecycle[0], "resolveArtifacts");
  assert.equal(
    setup.calls.lifecycle.filter((event) => event === "resolveArtifacts")
      .length,
    1,
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
  assert.equal(
    modelRetry.calls.lifecycle.filter((event) => event === "resolveArtifacts")
      .length,
    1,
  );
  assert.equal(
    new Set(modelRetry.calls.model.map((options) => options.modelPath)).size,
    1,
  );
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
