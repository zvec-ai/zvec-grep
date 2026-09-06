import assert from "node:assert/strict";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import test from "node:test";
import { Model2VecEmbeddingModel } from "../../../dist/engine/models/backends/model2vec.js";
import { createTemporaryDirectory } from "../../helpers/fixtures.mjs";

function entry(overrides = {}) {
  const repo = overrides.repo ?? "test/potion";
  const revision = overrides.revision ?? "0123456789abcdef";
  return {
    backend: "model2vec",
    reference: "local/test-potion",
    provider: "local",
    model: "test-potion",
    repo,
    revision,
    sources: {
      huggingFace: { repo, revision },
      modelScope: {
        repo: `mirror/${repo}`,
        revision: "fedcba9876543210",
      },
    },
    artifacts: [
      {
        path: "model.safetensors",
        size: 8,
        sha256: "a".repeat(64),
      },
      {
        path: "tokenizer.json",
        size: 8,
        sha256: "b".repeat(64),
      },
    ],
    modelFile: "model.safetensors",
    embeddingTensor: "embeddings",
    tokenizerFile: "tokenizer.json",
    dimension: 3,
    metric: "cosine",
    normalize: true,
    queryPrefix: "query: ",
    documentPrefix: "passage: ",
    maxInputTokens: 512,
    maxBatchSize: 32,
    defaultConcurrency: 2,
    ...overrides,
  };
}

function createArtifactResolver(onResolve) {
  return async (options) => {
    onResolve?.(options);
    const source = options.sources[0];
    const paths = Object.fromEntries(
      options.artifacts.map((artifact) => [
        artifact.path,
        join(
          source.cacheDirectory,
          source.localPaths?.[artifact.path] ?? artifact.path,
        ),
      ]),
    );
    await Promise.all(
      Object.values(paths).map((path) =>
        mkdir(dirname(path), { recursive: true }),
      ),
    );
    return { source, directory: source.cacheDirectory, paths };
  };
}

async function writeSafetensors(path, dtype, values, shape) {
  const bytesPerValue = dtype === "F16" ? 2 : 4;
  let header = JSON.stringify({
    embeddings: {
      dtype,
      shape,
      data_offsets: [0, values.length * bytesPerValue],
    },
  });
  while (Buffer.byteLength(header) % 8 !== 0) {
    header += " ";
  }

  const headerBytes = Buffer.from(header);
  const prefix = Buffer.alloc(8);
  prefix.writeBigUInt64LE(BigInt(headerBytes.length));
  const data = Buffer.alloc(values.length * bytesPerValue);
  values.forEach((value, index) => {
    if (dtype === "F16") {
      data.writeUInt16LE(value, index * bytesPerValue);
    } else {
      data.writeFloatLE(value, index * bytesPerValue);
    }
  });

  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, Buffer.concat([prefix, headerBytes, data]));
}

test("Model2Vec downloads pinned Safetensors assets and performs normalized static lookup", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-model2vec-");
  const calls = { resolutions: [], tokenizerLoads: [], tableLoads: [] };
  const downloadProgress = [];
  const tokenizer = Object.assign(
    async (text, options) => {
      calls.tokenizerCalls ??= [];
      calls.tokenizerCalls.push({ text, options });
      return {
        input_ids: {
          data: BigInt64Array.from(
            text.includes("unknown-only")
              ? [99n]
              : text.includes("both")
                ? [0n, 1n]
                : [2n],
          ),
        },
      };
    },
    { unk_token_id: 99 },
  );
  const dependencies = {
    async loadTokenizer(source, options) {
      calls.tokenizerLoads.push({ source, options });
      return tokenizer;
    },
    async loadSafetensors(path, tensorName, dimension) {
      calls.tableLoads.push({ path, tensorName, dimension });
      return {
        data: Float32Array.from([1, 0, 0, 0, 1, 0, 0, 0, 2]),
        dimension: 3,
        dtype: "F32",
        rows: 3,
      };
    },
    async resolveArtifacts(options) {
      calls.resolutions.push(options);
      options.onDownloadPlan?.(options.artifacts);
      options.onProgress?.({
        model: options.model,
        source: "huggingface",
        artifact: options.artifacts[0].path,
        downloadedBytes: 0,
        totalBytes: options.artifacts[0].size,
      });
      for (const artifact of options.artifacts) {
        options.onProgress?.({
          model: options.model,
          source: "huggingface",
          artifact: artifact.path,
          downloadedBytes: 4,
          totalBytes: 8,
        });
      }
      return await createArtifactResolver()(options);
    },
  };

  const model = new Model2VecEmbeddingModel(
    entry(),
    {
      apiKey: "",
      modelCacheDir: root,
    },
    dependencies,
  );
  assert.equal(model.info.defaultConcurrency, 2);
  await model.prepare({
    onProgress: (progress) => downloadProgress.push(progress),
  });
  assert.equal(calls.tokenizerCalls, undefined);
  assert.equal(calls.tableLoads.length, 1);
  assert.equal(calls.tokenizerLoads.length, 1);
  await model.prepare({
    onProgress: (progress) => downloadProgress.push(progress),
  });
  const { vectors } = await model.embed(
    [
      { kind: "text", text: "both tokens" },
      { kind: "text", text: "unknown-only" },
      { kind: "text", text: "third token" },
    ],
    {
      purpose: "query",
      onProgress: (progress) => downloadProgress.push(progress),
    },
  );

  assert.ok(Math.abs(vectors[0][0] - Math.SQRT1_2) < 1e-7);
  assert.ok(Math.abs(vectors[0][1] - Math.SQRT1_2) < 1e-7);
  assert.deepEqual(vectors[0].slice(2), [0]);
  assert.deepEqual(vectors[1], [0, 0, 0]);
  assert.deepEqual(vectors[2], [0, 0, 1]);
  assert.deepEqual(
    calls.tokenizerCalls.map(({ text }) => text),
    ["query: both tokens", "query: unknown-only", "query: third token"],
  );
  assert.deepEqual(calls.tokenizerCalls[0].options, {
    add_special_tokens: false,
    truncation: true,
    max_length: 513,
  });
  assert.equal(calls.resolutions.length, 1);
  const { onDownloadPlan, onProgress, onFallback, ...resolution } =
    calls.resolutions[0];
  assert.equal(typeof onDownloadPlan, "function");
  assert.equal(typeof onProgress, "function");
  assert.equal(typeof onFallback, "function");
  assert.deepEqual(resolution, {
    model: "local/test-potion",
    sources: [
      {
        kind: "huggingface",
        repo: "test/potion",
        revision: "0123456789abcdef",
        cacheDirectory: join(
          root,
          "model2vec",
          "test--potion",
          "0123456789abcdef",
        ),
        localPaths: {
          "model.safetensors": "model.safetensors",
          "tokenizer.json": "tokenizer/tokenizer.json",
        },
      },
      {
        kind: "modelscope",
        repo: "mirror/test/potion",
        revision: "fedcba9876543210",
        cacheDirectory: join(
          root,
          "modelscope",
          "model2vec",
          "mirror--test--potion",
          "fedcba9876543210",
        ),
        localPaths: {
          "model.safetensors": "model.safetensors",
          "tokenizer.json": "tokenizer/tokenizer.json",
        },
      },
    ],
    artifacts: entry().artifacts,
  });
  assert.deepEqual(downloadProgress, [
    {
      stage: "preparing",
      model: "local/test-potion",
    },
    {
      stage: "downloading",
      model: "local/test-potion",
      downloadedBytes: 0,
      totalBytes: 16,
    },
    {
      stage: "downloading",
      model: "local/test-potion",
      downloadedBytes: 4,
      totalBytes: 16,
    },
    {
      stage: "downloading",
      model: "local/test-potion",
      downloadedBytes: 8,
      totalBytes: 16,
    },
    {
      stage: "ready",
      model: "local/test-potion",
    },
  ]);
  assert.equal(calls.tokenizerLoads[0].options.local_files_only, true);
  assert.match(calls.tokenizerLoads[0].source, /tokenizer$/);
  assert.deepEqual(calls.tableLoads[0], {
    path: join(
      root,
      "model2vec",
      "test--potion",
      "0123456789abcdef",
      "model.safetensors",
    ),
    tensorName: "embeddings",
    dimension: 3,
  });

  await model.embed([{ kind: "text", text: "cached" }]);
  assert.equal(calls.resolutions.length, 1);
  assert.equal(calls.tokenizerLoads.length, 1);
  assert.equal(calls.tableLoads.length, 1);

  await model.dispose();
  await model.dispose();
  await assert.rejects(
    model.embed([{ kind: "text", text: "after dispose" }]),
    /disposed/,
  );
});

test("Model2Vec preparation respects cancellation and disposal before loading", async () => {
  const model = new Model2VecEmbeddingModel(
    entry(),
    { modelCacheDir: "/unused" },
    {
      async resolveArtifacts() {
        assert.fail("cancelled or disposed preparation must not download");
      },
    },
  );
  const cancelled = new Error("cancelled before model preparation");
  const progress = [];
  await assert.rejects(
    model.prepare({
      signal: AbortSignal.abort(cancelled),
      onProgress: (event) => progress.push(event),
    }),
    (error) => error === cancelled,
  );
  assert.deepEqual(progress, []);

  await model.dispose();
  await assert.rejects(
    model.prepare(),
    (error) => error.code === "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_DISPOSED",
  );
});

test("Model2Vec parses real F32 and F16 Safetensors embedding tables", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-model2vec-safetensors-");
  const dependencies = {
    async loadTokenizer() {
      return Object.assign(
        async (text) => ({
          input_ids: {
            data: BigInt64Array.from([text.includes("second") ? 1n : 0n]),
          },
        }),
        { unk_token_id: 99 },
      );
    },
    resolveArtifacts: createArtifactResolver(),
  };

  const fixtures = [
    {
      dtype: "F32",
      repo: "test/f32",
      values: [1, 2, 3, 4, 5, 6],
    },
    {
      dtype: "F16",
      repo: "test/f16",
      values: [0x3c00, 0x4000, 0x4200, 0x4400, 0x4500, 0x4600],
    },
  ];

  for (const fixture of fixtures) {
    const cacheDirectory = join(
      root,
      "model2vec",
      fixture.repo.replaceAll("/", "--"),
      "0123456789abcdef",
    );
    await writeSafetensors(
      join(cacheDirectory, "model.safetensors"),
      fixture.dtype,
      fixture.values,
      [2, 3],
    );
    await mkdir(join(cacheDirectory, "tokenizer"), { recursive: true });
    await writeFile(join(cacheDirectory, "tokenizer", "tokenizer.json"), "{}");

    const model = new Model2VecEmbeddingModel(
      entry({
        id: `local/${fixture.dtype.toLowerCase()}`,
        model: fixture.dtype.toLowerCase(),
        repo: fixture.repo,
        normalize: false,
      }),
      {
        apiKey: "",
        modelCacheDir: root,
      },
      dependencies,
    );
    assert.deepEqual(
      await model.embed([
        { kind: "text", text: "first" },
        { kind: "text", text: "second" },
      ]),
      {
        vectors: [
          [1, 2, 3],
          [4, 5, 6],
        ],
        truncated: [],
      },
    );
    await model.dispose();
  }
});

test("Model2Vec excludes cached artifacts from overall download progress", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-model2vec-partial-");
  const tokenizerPath = join(
    root,
    "model2vec",
    "test--potion",
    "0123456789abcdef",
    "tokenizer",
    "tokenizer.json",
  );
  await mkdir(dirname(tokenizerPath), { recursive: true });
  await writeFile(tokenizerPath, "{}");
  const tokenizerConfigPath = join(
    dirname(tokenizerPath),
    "tokenizer_config.json",
  );
  await writeFile(tokenizerConfigPath, "{");

  const dependencies = {
    async loadTokenizer() {
      return Object.assign(
        async () => ({ input_ids: { data: BigInt64Array.from([0n]) } }),
        { unk_token_id: 99 },
      );
    },
    async loadSafetensors() {
      return {
        data: Float32Array.from([1, 0, 0]),
        dimension: 3,
        dtype: "F32",
        rows: 1,
      };
    },
    async resolveArtifacts(options) {
      options.onDownloadPlan?.([options.artifacts[0]]);
      options.onProgress?.({
        model: options.model,
        source: "huggingface",
        artifact: "model.safetensors",
        downloadedBytes: 0,
        totalBytes: 8,
      });
      options.onProgress?.({
        model: options.model,
        source: "huggingface",
        artifact: "model.safetensors",
        downloadedBytes: 4,
        totalBytes: 8,
      });
      return await createArtifactResolver()(options);
    },
  };
  const model = new Model2VecEmbeddingModel(
    entry(),
    { modelCacheDir: root },
    dependencies,
  );
  const progress = [];

  await model.embed([{ kind: "text", text: "cached tokenizer" }], {
    onProgress: (event) => progress.push(event),
  });

  assert.deepEqual(progress, [
    {
      stage: "preparing",
      model: "local/test-potion",
    },
    {
      stage: "downloading",
      model: "local/test-potion",
      downloadedBytes: 0,
      totalBytes: 8,
    },
    {
      stage: "downloading",
      model: "local/test-potion",
      downloadedBytes: 4,
      totalBytes: 8,
    },
    {
      stage: "ready",
      model: "local/test-potion",
    },
  ]);
  assert.deepEqual(JSON.parse(await readFile(tokenizerConfigPath, "utf8")), {
    tokenizer_class: "PreTrainedTokenizer",
  });
  await model.dispose();
});

test("Model2Vec reports download failures without exposing artifact names", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-model2vec-failure-");
  const progress = [];
  const model = new Model2VecEmbeddingModel(
    entry(),
    { modelCacheDir: root },
    {
      async loadTokenizer() {
        throw new Error("tokenizer should not load after a download failure");
      },
      async loadSafetensors() {
        throw new Error("table should not load after a download failure");
      },
      async resolveArtifacts(options) {
        options.onDownloadPlan?.(options.artifacts);
        options.onProgress?.({
          model: options.model,
          source: "huggingface",
          artifact: options.artifacts[0].path,
          downloadedBytes: 0,
          totalBytes: options.artifacts[0].size,
        });
        throw new Error("network unavailable");
      },
    },
  );

  let failure;
  await assert.rejects(
    model.embed([{ kind: "text", text: "download failure" }], {
      onProgress: (event) => progress.push(event),
    }),
    (error) => {
      failure = error;
      return error.code === "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_DOWNLOAD_FAILED";
    },
  );

  assert.deepEqual(
    progress.map(({ stage }) => stage),
    ["preparing", "downloading", "warning"],
  );
  assert.deepEqual(progress[1], {
    stage: "downloading",
    model: "local/test-potion",
    downloadedBytes: 0,
    totalBytes: 16,
  });
  assert.match(progress[2].message, /network access.*model cache/i);
  assert.doesNotMatch(
    JSON.stringify(progress),
    /model\.safetensors|tokenizer\.json/,
  );
  assert.doesNotMatch(failure.context, /model\.safetensors|tokenizer\.json/);
});

test("Model2Vec classifies table preparation errors as model load failures", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-model2vec-load-");
  const progress = [];
  const model = new Model2VecEmbeddingModel(
    entry(),
    { modelCacheDir: root },
    {
      async loadTokenizer() {
        throw new Error("tokenizer should not load after a table failure");
      },
      async loadSafetensors() {
        throw new Error("invalid static table");
      },
      resolveArtifacts: createArtifactResolver(),
    },
  );

  await assert.rejects(
    model.embed([{ kind: "text", text: "load failure" }], {
      onProgress: (event) => progress.push(event),
    }),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_LOAD_FAILED" &&
      error.cause?.message === "invalid static table",
  );
  assert.equal(progress.at(-1)?.stage, "warning");
});

test("Model2Vec reports and truncates inputs beyond the model token limit", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-model2vec-truncate-");
  const calls = [];
  const dependencies = {
    async loadTokenizer() {
      return Object.assign(
        async (_text, options) => {
          calls.push(options);
          return {
            input_ids: { data: BigInt64Array.from([0n, 1n, 2n]) },
          };
        },
        { unk_token_id: 99 },
      );
    },
    async loadSafetensors() {
      return {
        data: Float32Array.from([1, 0, 0, 0, 1, 0, 0, 0, 10]),
        dimension: 3,
        dtype: "F32",
        rows: 3,
      };
    },
    resolveArtifacts: createArtifactResolver(),
  };

  const model = new Model2VecEmbeddingModel(
    entry({ maxInputTokens: 2 }),
    {
      apiKey: "",
      modelCacheDir: root,
    },
    dependencies,
  );
  const result = await model.embed([{ kind: "text", text: "too many tokens" }]);

  assert.deepEqual(result.truncated, [0]);
  assert.ok(Math.abs(result.vectors[0][0] - Math.SQRT1_2) < 1e-7);
  assert.ok(Math.abs(result.vectors[0][1] - Math.SQRT1_2) < 1e-7);
  assert.equal(result.vectors[0][2], 0);
  assert.deepEqual(calls, [
    {
      add_special_tokens: false,
      truncation: true,
      max_length: 3,
    },
  ]);
});

test("Model2Vec rejects token ids outside the static embedding table", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-model2vec-invalid-");
  const dependencies = {
    async loadTokenizer() {
      return Object.assign(
        async () => ({ input_ids: { data: BigInt64Array.from([3n]) } }),
        { unk_token_id: 99 },
      );
    },
    async loadSafetensors() {
      return {
        data: Float32Array.from([1, 0, 0, 0, 1, 0, 0, 0, 1]),
        dimension: 3,
        dtype: "F32",
        rows: 3,
      };
    },
    resolveArtifacts: createArtifactResolver(),
  };

  const model = new Model2VecEmbeddingModel(
    entry(),
    {
      apiKey: "",
      modelCacheDir: root,
    },
    dependencies,
  );
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    (error) =>
      error.message === "Model2Vec embedding failed" &&
      error.cause?.message.includes("out-of-range token id"),
  );
});

test("Model2Vec reuses a loaded worker pool without reloading artifacts", async () => {
  const model = new Model2VecEmbeddingModel(
    entry(),
    { modelCacheDir: "/unused" },
    {
      async loadTokenizer() {
        throw new Error(
          "worker-backed model should not load in the main thread",
        );
      },
      async loadSafetensors() {
        throw new Error("worker-backed model should not reload its table");
      },
      async resolveArtifacts() {
        throw new Error("worker-backed model should not resolve artifacts");
      },
    },
  );
  let runs = 0;
  let disposed = false;
  model.workerPool = {
    async run(texts) {
      runs++;
      return {
        vectors: texts.map(() => [1, 0, 0]),
        truncated: [],
      };
    },
    async dispose() {
      disposed = true;
    },
  };

  await model.embed([{ kind: "text", text: "first" }]);
  await model.embed([{ kind: "text", text: "second" }]);
  assert.equal(runs, 2);
  await model.dispose();
  assert.equal(disposed, true);
});
