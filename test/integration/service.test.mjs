import assert from "node:assert/strict";
import { access, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { EngineError } from "../../dist/engine/errors.js";
import { Model2VecEmbeddingModel } from "../../dist/engine/models/backends/model2vec.js";
import { CURRENT_INDEX_VERSION } from "../../dist/engine/types.js";
import { createZvecGrep } from "../../dist/index.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";

class SelectivelyFailingEmbeddingModel extends FakeEmbeddingModel {
  constructor() {
    super();
    this.info = { ...this.info, provider: "qwen" };
  }

  async doEmbed(contents) {
    if (
      contents.some(
        (content) =>
          content.kind === "text" && content.text.includes("FailureNeedle"),
      )
    ) {
      throw new EngineError("fixture embedding failure", {
        code: "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR",
        context: "status=400 providerCode=invalid_input",
      });
    }
    return super.doEmbed(contents);
  }
}

class SharedFailureEmbeddingModel extends FakeEmbeddingModel {
  calls = 0;

  constructor({ provider, code, context }) {
    super();
    this.info = { ...this.info, provider };
    this.code = code;
    this.context = context;
  }

  async doEmbed() {
    this.calls++;
    throw new EngineError("shared embedding failure", {
      code: this.code,
      context: this.context,
    });
  }
}

class ImmediateAuthenticationFailureEmbeddingModel extends FakeEmbeddingModel {
  calls = 0;

  constructor() {
    super();
    this.info = {
      ...this.info,
      provider: "qwen",
      defaultConcurrency: 4,
      limits: { maxBatchSize: 1 },
    };
  }

  async doEmbed() {
    const call = ++this.calls;
    throw new EngineError(`fixture authentication failure call=${call}`, {
      code: "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR",
      context: `status=401 providerCode=invalid_api_key call=${call}`,
    });
  }
}

class RecoveringRemoteEmbeddingModel extends FakeEmbeddingModel {
  calls = 0;

  constructor(failure) {
    super();
    this.info = { ...this.info, provider: "qwen" };
    this.failure = failure;
  }

  async doEmbed(contents) {
    this.calls++;
    if (this.calls < 3) {
      throw this.failure();
    }
    return super.doEmbed(contents);
  }
}

class DownloadProgressEmbeddingModel extends FakeEmbeddingModel {
  started = false;
  finishDownload;

  constructor() {
    super();
    this.info = {
      ...this.info,
      defaultConcurrency: 2,
      limits: { maxBatchSize: 1 },
    };
  }

  async doEmbed(contents, options) {
    if (!this.started) {
      this.started = true;
      options.onProgress?.({
        stage: "preparing",
        model: "local/test-download",
      });
      options.onProgress?.({
        stage: "downloading",
        model: "local/test-download",
        downloadedBytes: 25,
        totalBytes: 100,
      });
      await new Promise((resolve) => {
        this.finishDownload = resolve;
      });
      options.onProgress?.({
        stage: "ready",
        model: "local/test-download",
      });
    } else {
      this.finishDownload?.();
    }
    return super.doEmbed(contents, options);
  }
}

function multiBatchModel2Vec(modelCacheDir, failure) {
  const calls = {
    downloads: [],
    loads: [],
    batches: [],
    completedBatches: [],
    events: [],
    activeBatches: 0,
    maxActiveBatches: 0,
    failure,
  };
  let releaseFailedDownload;
  let releaseFirstBatch;
  const firstBatchMayComplete = new Promise((resolve) => {
    releaseFirstBatch = resolve;
  });
  class TrackedModel2Vec extends Model2VecEmbeddingModel {
    async doEmbed(contents, options) {
      const batch = calls.batches.length;
      calls.batches.push(contents.map((content) => content.text));
      calls.events.push("embed");
      calls.activeBatches++;
      calls.maxActiveBatches = Math.max(
        calls.maxActiveBatches,
        calls.activeBatches,
      );
      try {
        const result = await super.doEmbed(contents, options);
        if (batch === 0) {
          // Complete a later batch first to exercise vector/fragment ordering.
          await firstBatchMayComplete;
        } else if (batch === 1) {
          releaseFirstBatch();
        }
        calls.completedBatches.push(batch);
        return result;
      } finally {
        calls.activeBatches--;
      }
    }
  }
  const model = new TrackedModel2Vec(
    {
      backend: "model2vec",
      reference: "local/test-multi-batch-potion",
      provider: "local",
      model: "test-multi-batch-potion",
      repo: "test/multi-batch-potion",
      revision: "0123456789abcdef",
      modelFile: "model.safetensors",
      embeddingTensor: "embeddings",
      tokenizerFile: "tokenizer.json",
      dimension: 3,
      metric: "cosine",
      normalize: true,
      maxInputTokens: 512,
      maxBatchSize: 256,
      defaultConcurrency: 2,
    },
    { modelCacheDir },
    {
      async download(url, destination) {
        calls.downloads.push(url.split("/").at(-1));
        calls.events.push("download");
        if (calls.failure === "download") {
          // Both artifacts belong to one preparation attempt. Let both start
          // before failing so the count does not depend on filesystem timing.
          if (calls.downloads.length % 2 === 1) {
            await new Promise((resolve) => {
              releaseFailedDownload = resolve;
            });
          } else {
            releaseFailedDownload();
          }
          throw new Error("HTTP 503 Service Unavailable");
        }
        await writeFile(destination, "fixture model asset");
      },
      async loadSafetensors() {
        calls.loads.push("table");
        calls.events.push("load:table");
        if (calls.failure === "load") {
          throw new Error("invalid model tensor");
        }
        return {
          data: Float32Array.from(
            Array.from({ length: 770 }, (_, id) => [
              Math.cos(id / 250),
              Math.sin(id / 250),
              1,
            ]).flat(),
          ),
          dimension: 3,
          dtype: "F32",
          rows: 770,
        };
      },
      async loadTokenizer() {
        calls.loads.push("tokenizer");
        calls.events.push("load:tokenizer");
        return async (text) => ({
          input_ids: {
            data: [Number(/\bFragment(\d+)\b/.exec(text)?.[1] ?? 769)],
          },
        });
      },
    },
  );
  return { model, calls };
}

test("service exposes embedding model download progress while indexing", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-download-progress-",
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "example.ts"), "export const Example = 1;\n");
  await writeFile(join(root, "second.ts"), "export const Second = 2;\n");

  const service = await createZvecGrep({
    root,
    embeddingModel: new DownloadProgressEmbeddingModel(),
  });
  t.after(() => service.close());

  const progressEvents = [];
  await service.index({
    onProgress: (progress) => progressEvents.push(progress),
  });

  assert.ok(
    progressEvents.some(
      (progress) =>
        progress.phase === "indexing" &&
        progress.embedding?.stage === "downloading" &&
        progress.embedding.model === "local/test-download" &&
        progress.embedding.downloadedBytes === 25 &&
        progress.embedding.totalBytes === 100,
    ),
  );
  const preparingIndex = progressEvents.findIndex(
    (progress) => progress.embedding?.stage === "preparing",
  );
  const readyIndex = progressEvents.findIndex(
    (progress) => progress.embedding?.stage === "ready",
  );
  assert.ok(preparingIndex >= 0);
  assert.ok(readyIndex > preparingIndex);
  assert.ok(
    progressEvents.some(
      (progress) =>
        progress.embedding?.concurrency === 2 &&
        progress.embedding.maxConcurrency === 2,
    ),
  );
  assert.ok(
    progressEvents
      .slice(preparingIndex, readyIndex + 1)
      .some((progress) => progress.embedding?.stage === undefined),
  );
});

test("service fails fast when a shared local embedding model cannot be prepared", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-local-model-failure-",
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "first.ts"), "export const First = 1;\n");
  await writeFile(join(root, "second.ts"), "export const Second = 2;\n");
  const model = new SharedFailureEmbeddingModel({
    provider: "local",
    code: "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_DOWNLOAD_FAILED",
    context: "model=local/test-potion repo=test/potion status=503",
  });
  const service = await createZvecGrep({ root, embeddingModel: model });
  t.after(() => service.close());
  const progressEvents = [];

  await assert.rejects(
    service.index({
      onProgress: (progress) => progressEvents.push(progress),
    }),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_DOWNLOAD_FAILED",
  );

  assert.equal(model.calls, 1);
  assert.equal(
    progressEvents.some((progress) =>
      progress.detail?.toLowerCase().includes("retrying"),
    ),
    false,
  );
});

test("service prepares Model2Vec once before queuing a large file and can recover on a later index", async (t) => {
  for (const failure of ["download", "load"]) {
    await t.test(failure, { timeout: 60_000 }, async (t) => {
      const temporaryDirectory = await createTemporaryDirectory(
        t,
        "zvec-grep-multi-batch-model-preparation-",
      );
      const root = join(temporaryDirectory, "repo");
      await mkdir(root, { recursive: true });
      await writeFile(
        join(root, "large.ts"),
        Array.from(
          { length: 769 },
          (_, index) =>
            `export function Fragment${index}() { return ${index}; }\n`,
        ).join(""),
      );
      const { model, calls } = multiBatchModel2Vec(
        join(temporaryDirectory, "models"),
        failure,
      );
      const service = await createZvecGrep({ root, embeddingModel: model });
      t.after(() => service.close());

      await assert.rejects(
        service.index({ embeddingConcurrency: 2 }),
        (error) =>
          error.code ===
          `ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_${failure.toUpperCase()}_FAILED`,
      );
      assert.deepEqual(calls.downloads.toSorted(), [
        "model.safetensors",
        "tokenizer.json",
      ]);
      assert.deepEqual(calls.loads, failure === "load" ? ["table"] : []);
      assert.deepEqual(calls.batches, []);

      calls.failure = null;
      calls.events.length = 0;
      const recovered = await service.index({ embeddingConcurrency: 2 });

      assert.equal(recovered.filesFailed, 0);
      assert.equal(recovered.filesScanned, 1);
      assert.ok(recovered.entitiesCreated >= 769);
      assert.equal(calls.downloads.length, failure === "download" ? 4 : 2);
      assert.deepEqual(
        calls.loads,
        failure === "load"
          ? ["table", "table", "tokenizer"]
          : ["table", "tokenizer"],
      );
      assert.ok(calls.batches.length >= 4);
      assert.equal(calls.batches[0].length, 256);
      assert.equal(calls.maxActiveBatches, 2);
      assert.notEqual(calls.completedBatches[0], 0);
      assert.ok(
        calls.events.indexOf("load:tokenizer") < calls.events.indexOf("embed"),
      );

      for (const index of [0, 256, 512, 768]) {
        const result = await service.context({
          routes: [{ mode: "vector", query: `Fragment${index}` }],
          autoUpdate: false,
          limit: 1,
        });
        assert.equal(result.items[0]?.metadata?.symbolName, `Fragment${index}`);
      }
    });
  }
});

test("service only prepares local models for changed embeddable content", async (t) => {
  for (const provider of ["local", "qwen"]) {
    await t.test(provider, async (t) => {
      const temporaryDirectory = await createTemporaryDirectory(
        t,
        "zvec-grep-lazy-model-preparation-",
      );
      const root = join(temporaryDirectory, "repo");
      await mkdir(root, { recursive: true });
      const model = new FakeEmbeddingModel();
      model.info = { ...model.info, provider };
      let preparations = 0;
      model.prepare = async () => {
        preparations++;
      };
      const service = await createZvecGrep({ root, embeddingModel: model });
      t.after(() => service.close());

      await service.index();
      assert.equal(preparations, 0);
      await writeFile(join(root, "empty.ts"), "");
      await service.index();
      assert.equal(preparations, 0);
      await writeFile(join(root, "example.ts"), "export const Example = 1;\n");
      await service.index();
      assert.equal(preparations, provider === "local" ? 1 : 0);
      await service.index();
      assert.equal(preparations, provider === "local" ? 1 : 0);
    });
  }
});

test("service fails fast on permanent remote authentication failures", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-remote-auth-failure-",
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "first.ts"), "export const First = 1;\n");
  await writeFile(join(root, "second.ts"), "export const Second = 2;\n");
  const model = new SharedFailureEmbeddingModel({
    provider: "qwen",
    code: "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR",
    context: "status=401 providerCode=invalid_api_key",
  });
  const service = await createZvecGrep({ root, embeddingModel: model });
  t.after(() => service.close());

  await assert.rejects(
    service.index(),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR",
  );
  assert.equal(model.calls, 1);
});

for (const [name, context] of [
  [
    "invalid model",
    "status=400 providerCode=invalid_model providerType=invalid_request_error providerMessage=Model does not exist",
  ],
  [
    "invalid dimensions",
    "status=400 providerCode=InvalidParameter providerType=invalid_request_error providerMessage=dimensions must be between 1 and 4096",
  ],
]) {
  test(`service fails fast on permanent remote ${name} failures`, async (t) => {
    const temporaryDirectory = await createTemporaryDirectory(
      t,
      "zvec-grep-remote-configuration-failure-",
    );
    const root = join(temporaryDirectory, "repo");
    await mkdir(root, { recursive: true });
    await writeFile(join(root, "example.ts"), "export const Example = 1;\n");
    const model = new SharedFailureEmbeddingModel({
      provider: "qwen",
      code: "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR",
      context,
    });
    const service = await createZvecGrep({ root, embeddingModel: model });
    t.after(() => service.close());

    await assert.rejects(
      service.index(),
      (error) =>
        error.code ===
          "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR" &&
        error.context === context,
    );
    assert.equal(model.calls, 1);
  });
}

test("service stops scheduling multi-file embedding batches after the first fatal rejection", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-fast-reject-multi-file-",
    { cleanup: false },
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root, { recursive: true });
  await Promise.all(
    Array.from({ length: 20 }, (_, index) =>
      writeFile(join(root, `file-${index}.txt`), `unique content ${index}\n`),
    ),
  );
  const model = new ImmediateAuthenticationFailureEmbeddingModel();
  const service = await createZvecGrep({ root, embeddingModel: model });
  t.after(async () => {
    await service.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  const unhandledRejections = [];
  const onUnhandledRejection = (reason) => unhandledRejections.push(reason);
  process.on("unhandledRejection", onUnhandledRejection);
  try {
    await assert.rejects(
      service.index({ embeddingConcurrency: 4 }),
      (error) =>
        error.code ===
          "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR" &&
        error.message === "fixture authentication failure call=1",
    );
    await new Promise((resolve) => setImmediate(resolve));
  } finally {
    process.off("unhandledRejection", onUnhandledRejection);
  }

  assert.ok(model.calls >= 1);
  assert.ok(
    model.calls <= 4,
    `expected at most the active concurrency, received ${model.calls} calls`,
  );
  assert.deepEqual(unhandledRejections, []);
});

test("service stops scheduling remaining fragment batches after a fatal rejection", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-fast-reject-multi-fragment-",
    { cleanup: false },
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root, { recursive: true });
  await writeFile(
    join(root, "large.ts"),
    Array.from(
      { length: 12 },
      (_, index) => `export function Fragment${index}() { return ${index}; }\n`,
    ).join(""),
  );
  const model = new ImmediateAuthenticationFailureEmbeddingModel();
  const service = await createZvecGrep({ root, embeddingModel: model });
  t.after(async () => {
    await service.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await assert.rejects(
    service.index({ embeddingConcurrency: 1 }),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR" &&
      error.message === "fixture authentication failure call=1",
  );
  assert.equal(model.calls, 1);
});

test("service stops after bounded remote retries without a failed-file pass", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-remote-retry-exhausted-",
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "example.ts"), "export const Example = 1;\n");
  const model = new SharedFailureEmbeddingModel({
    provider: "qwen",
    code: "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR",
    context: "status=503 retryAfterMs=0",
  });
  const service = await createZvecGrep({ root, embeddingModel: model });
  t.after(() => service.close());
  const progressEvents = [];

  await assert.rejects(
    service.index({
      onProgress: (progress) => progressEvents.push(progress),
    }),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR",
  );

  assert.equal(model.calls, 4);
  assert.equal(
    progressEvents.some((progress) =>
      progress.detail?.toLowerCase().includes("retrying"),
    ),
    false,
  );
});

test("service retries bounded remote HTTP and network failures before recovery", async (t) => {
  const fixtures = [
    {
      name: "rate-limit",
      failure: () =>
        new EngineError("remote rate limit", {
          code: "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR",
          context: "status=429 retryAfterMs=0",
        }),
    },
    {
      name: "request-timeout",
      failure: () =>
        new EngineError("remote request timeout", {
          code: "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR",
          context: "status=408 retryAfterMs=0",
        }),
    },
    {
      name: "server",
      failure: () =>
        new EngineError("remote server unavailable", {
          code: "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_API_ERROR",
          context: "status=503 retryAfterMs=0",
        }),
    },
    {
      name: "network",
      failure: () => {
        const cause = new Error("connection timed out");
        cause.code = "ETIMEDOUT";
        return new EngineError("remote request failed", {
          code: "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_REQUEST_FAILED",
          context: "model=qwen/test endpoint=https://example.test",
          cause,
        });
      },
    },
  ];

  for (const fixture of fixtures) {
    await t.test(fixture.name, async (t) => {
      const temporaryDirectory = await createTemporaryDirectory(
        t,
        `zvec-grep-remote-${fixture.name}-recovery-`,
      );
      const root = join(temporaryDirectory, "repo");
      await mkdir(root, { recursive: true });
      await writeFile(join(root, "example.ts"), "export const Example = 1;\n");
      const model = new RecoveringRemoteEmbeddingModel(fixture.failure);
      const service = await createZvecGrep({ root, embeddingModel: model });
      t.after(() => service.close());

      const result = await service.index();

      assert.equal(result.filesFailed, 0);
      assert.equal(model.calls, 3);
    });
  }
});

test("service indexes, searches, refreshes, and drops a workspace index", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-integration-",
  );
  const root = join(temporaryDirectory, "repo");
  const home = join(temporaryDirectory, "home");
  await mkdir(join(root, "src"), { recursive: true });
  await writeFile(
    join(root, "src", "alpha.ts"),
    "export const UniqueAlphaSymbol = 41;\n",
  );
  await writeFile(join(root, "src", "ignored.log"), "UniqueIgnoredSymbol\n");

  const service = await createZvecGrep({
    root,
    home,
    embeddingModel: new FakeEmbeddingModel(),
  });
  t.after(() => service.close());

  const indexed = await service.index({
    includePaths: ["src/**"],
    excludePaths: ["**/*.log"],
  });
  assert.equal(indexed.filesAdded, 1);
  assert.equal((await service.info()).indexed, true);

  const first = await service.context({
    root,
    query: "UniqueAlphaSymbol",
    limit: 5,
  });
  assert.ok(
    first.items.some((item) => item.file.relativePath.endsWith("alpha.ts")),
  );
  assert.equal(
    first.items.some((item) => item.file.relativePath.endsWith("ignored.log")),
    false,
  );

  await writeFile(
    join(root, "src", "alpha.ts"),
    "export const UniqueUpdatedSymbol = 42;\n",
  );
  const refreshed = await service.context({
    root,
    query: "UniqueUpdatedSymbol",
    limit: 5,
  });
  assert.ok(
    refreshed.items.some((item) => item.file.relativePath.endsWith("alpha.ts")),
  );

  assert.equal(await service.dropIndex(), true);
  assert.equal(await service.dropIndex(), false);
  assert.equal((await service.info()).indexed, false);
});

test("service optionally fuses independent query groups into one result list", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-fused-context-",
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "alpha.ts"), "export const AlphaNeedle = 1;\n");
  await writeFile(join(root, "beta.ts"), "export const BetaNeedle = 2;\n");
  for (const [name, value] of [
    ["gamma", "GammaNeedle"],
    ["delta", "DeltaNeedle"],
    ["epsilon", "EpsilonNeedle"],
    ["zeta", "ZetaNeedle"],
    ["eta", "EtaNeedle"],
  ]) {
    await writeFile(join(root, `${name}.ts`), `export const ${value} = 1;\n`);
  }

  const service = await createZvecGrep({
    root,
    embeddingModel: new FakeEmbeddingModel(),
  });
  t.after(() => service.close());
  await service.index();

  const routes = [
    { mode: "fts", query: "AlphaNeedle" },
    { mode: "fts", query: "BetaNeedle" },
  ];
  const grouped = await service.context({ routes, limit: 1 });
  const fused = await service.context({ routes, limit: 1, fuse: true });
  const duplicateGroups = await service.context({
    routes: [routes[0], routes[0]],
    limit: 1,
  });
  const mixedDuplicateGroups = await service.context({
    routes: [
      { mode: "vector", query: "AlphaNeedle" },
      { mode: "fts", query: "AlphaNeedle" },
    ],
    globs: ["alpha.ts"],
    limit: 1,
  });
  const primaryGroups = await service.context({
    queries: ["AlphaNeedle", "BetaNeedle"],
    limit: 1,
  });
  const cappedPrimaryGroups = await service.context({
    queries: [
      "AlphaNeedle",
      "BetaNeedle",
      "GammaNeedle",
      "DeltaNeedle",
      "EpsilonNeedle",
      "ZetaNeedle",
      "EtaNeedle",
    ],
    limit: 1,
  });

  assert.equal(grouped.items.length, 2);
  assert.deepEqual(
    grouped.diagnostics.index?.queryGroups?.map((group) => group.id),
    ["Q1", "Q2"],
  );
  assert.deepEqual(
    grouped.diagnostics.index?.queryGroups?.map((group) => group.role),
    ["supplemental", "supplemental"],
  );
  assert.deepEqual(
    grouped.items.map((item) => [item.selectionReason, item.coverageGroup]),
    [
      ["global_fill", undefined],
      ["global_fill", undefined],
    ],
  );
  assert.deepEqual(
    primaryGroups.diagnostics.index?.queryGroups?.map((group) => group.role),
    ["primary", "primary"],
  );
  assert.deepEqual(
    primaryGroups.items.map((item) => [
      item.selectionReason,
      item.coverageGroup,
    ]),
    [
      ["coverage", "Q1"],
      ["coverage", "Q2"],
    ],
  );
  assert.equal(
    cappedPrimaryGroups.items.filter(
      (item) => item.selectionReason !== undefined,
    ).length,
    6,
  );
  assert.equal(
    cappedPrimaryGroups.items.filter(
      (item) => item.selectionReason === "coverage",
    ).length,
    6,
  );
  assert.equal(fused.items.length, 1);
  assert.equal(fused.diagnostics.index?.routes.length, 2);
  assert.equal(fused.diagnostics.index?.queryGroups?.length, 1);
  assert.equal(duplicateGroups.items.length, 1);
  assert.deepEqual(
    duplicateGroups.groupResults?.map((group) => [
      group.id,
      group.items.length,
      group.items[0]?.rank,
    ]),
    [
      ["Q1", 1, 1],
      ["Q2", 1, 1],
    ],
  );
  assert.equal(
    duplicateGroups.groupResults?.[0]?.items[0]?.entityId,
    duplicateGroups.groupResults?.[1]?.items[0]?.entityId,
  );
  assert.deepEqual(
    mixedDuplicateGroups.groupResults?.map((group) => [
      group.id,
      group.items[0]?.rank,
      group.items[0]?.matchedBy,
    ]),
    [
      ["Q1", 1, "vector"],
      ["Q2", 1, "fts"],
    ],
  );
  assert.equal(
    mixedDuplicateGroups.groupResults?.[0]?.items[0]?.entityId,
    mixedDuplicateGroups.groupResults?.[1]?.items[0]?.entityId,
  );
  assert.equal(fused.groupResults?.length, 1);
  assert.deepEqual(
    duplicateGroups.items[0]?.queryGroups?.map((group) => [
      group.id,
      group.rank,
      group.role,
    ]),
    [
      ["Q1", 1, "supplemental"],
      ["Q2", 1, "supplemental"],
    ],
  );
});

test("workspace rebuild recreates unsupported index metadata", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-version-rebuild-",
  );
  const root = join(temporaryDirectory, "repo");
  const workspaceHome = join(root, ".zvec-grep");
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "legacy.ts"), "export const LegacyNeedle = 42;\n");

  let service = await createZvecGrep({
    root,
    embeddingModel: new FakeEmbeddingModel(),
  });
  t.after(async () => {
    await service.close();
  });
  await service.index();
  await service.close();

  const manifestPath = join(workspaceHome, "manifest.json");
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  await writeFile(
    manifestPath,
    `${JSON.stringify({ ...manifest, indexVersion: 999 }, null, 2)}\n`,
  );

  const filesMarker = join(workspaceHome, "files.zvec", "legacy-marker");
  const indexMarker = join(workspaceHome, "index.zvec", "legacy-marker");
  const authorizationPath = join(workspaceHome, "authorization.json");
  await writeFile(filesMarker, "legacy");
  await writeFile(indexMarker, "legacy");
  await writeFile(authorizationPath, "preserve");

  service = await createZvecGrep({
    root,
    embeddingModel: new FakeEmbeddingModel(),
  });
  await assert.rejects(
    service.info(),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.VERSION_MISMATCH" &&
      error.context.includes("zg --index --rebuild"),
  );

  await service.index({ rebuild: true });
  const rebuilt = await service.info();
  assert.equal(rebuilt.workspaceIndex?.indexVersion, CURRENT_INDEX_VERSION);
  await assert.rejects(access(filesMarker), { code: "ENOENT" });
  await assert.rejects(access(indexMarker), { code: "ENOENT" });
  assert.equal(await readFile(authorizationPath, "utf8"), "preserve");

  const result = await service.context({
    routes: [{ mode: "fts", query: "LegacyNeedle" }],
    autoUpdate: false,
  });
  assert.ok(result.items.length > 0);
});

test("service records failed files, retries them, deletes stale records, and rebuilds", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-index-failures-",
  );
  const root = join(temporaryDirectory, "repo");
  const home = join(temporaryDirectory, "home");
  await mkdir(root, { recursive: true });
  const goodPath = join(root, "good.ts");
  const failingPath = join(root, "failing.ts");
  await writeFile(goodPath, "export const GoodNeedle = 1;\n");
  await writeFile(failingPath, "export const FailureNeedle = 2;\n");

  const service = await createZvecGrep({
    root,
    home,
    embeddingModel: new SelectivelyFailingEmbeddingModel(),
  });
  t.after(() => service.close());

  const completedFiles = [];
  const retryScanningProgress = [];
  let retryStarted = false;
  await assert.rejects(
    service.index({
      embeddingConcurrency: 2,
      onProgress: (progress) => {
        if (
          progress.phase === "scanning" &&
          progress.detail?.toLowerCase().includes("retry")
        ) {
          retryStarted = true;
        }
        if (retryStarted && progress.phase === "scanning") {
          retryScanningProgress.push(progress);
        }
        if (progress.filesIndexed !== undefined) {
          completedFiles.push(
            progress.filesIndexed - (progress.filesFailed ?? 0),
          );
        }
      },
    }),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED" &&
      /failing\.ts/.test(error.context),
  );
  assert.ok(
    completedFiles.every(
      (completed, index) =>
        index === 0 || completed >= completedFiles[index - 1],
    ),
  );
  assert.ok(retryScanningProgress.length > 0);
  assert.ok(
    retryScanningProgress.every(
      (progress) =>
        progress.filesIndexed !== undefined &&
        progress.filesTotal !== undefined,
    ),
  );
  const failedStatus = await service.info();
  assert.equal(failedStatus.status?.filesFailed, 1);
  assert.equal(failedStatus.status?.filesIndexed, 1);
  assert.match(
    failedStatus.status?.failedFiles[0].indexStatus?.error ?? "",
    /fixture embedding failure/,
  );

  await writeFile(failingPath, "export const RecoveredNeedle = 3;\n");
  const retried = await service.index();
  assert.equal(retried.filesFailed, 0);
  assert.equal(retried.filesPending + retried.filesModified >= 1, true);
  assert.equal((await service.info()).status?.filesIndexed, 2);

  await rm(goodPath);
  const deleted = await service.index();
  assert.equal(deleted.filesDeleted, 1);
  assert.equal((await service.info()).status?.filesIndexed, 1);

  const rebuilt = await service.index({ rebuild: true });
  assert.equal(rebuilt.filesScanned, 1);
  assert.equal(rebuilt.filesAdded, 1);
});

test("changedPaths preserves request-specific embedding failure details", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-path-failure-details-",
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root, { recursive: true });
  const changedPath = join(root, "changed.ts");
  await writeFile(changedPath, "export const InitialNeedle = 1;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new SelectivelyFailingEmbeddingModel(),
  });
  t.after(() => service.close());
  await service.index();

  await writeFile(changedPath, "export const FailureNeedle = 2;\n");
  await assert.rejects(
    service.index({ changedPaths: [changedPath] }),
    (error) =>
      error.code === "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED" &&
      /failedFiles=changed\.ts/.test(error.context) &&
      /QWEN_TEXT_EMBEDDING_API_ERROR/.test(error.context) &&
      /fixture embedding failure/.test(error.context) &&
      /status=400/.test(error.context) &&
      /providerCode=invalid_input/.test(error.context) &&
      /Retried failed files once automatically/.test(error.context),
  );
});
