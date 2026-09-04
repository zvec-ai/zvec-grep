import assert from "node:assert/strict";
import { writeFileSync } from "node:fs";
import { mkdir, mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { DaemonBackend } from "../dist/daemon/backend.js";
import { inspectRoot } from "../dist/daemon/runtime-manager.js";
import { BaseEmbeddingModel } from "../dist/engine/models/embeddings.js";
import { createZvecGrep } from "../dist/index.js";

const noopWatchManagerFactory = () => ({
  start() {},
  flushPending: async () => {},
  close: async () => {},
});

test("index releases its model lease when service creation fails", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-backend-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const backend = new DaemonBackend({
    version: "1.0.0",
    watchManagerFactory: noopWatchManagerFactory,
    modelPoolOptions: {
      createModel: () => new TestEmbeddingModel(),
    },
    createService: async () => {
      throw new Error("service creation failed");
    },
  });
  try {
    const result = await backend.index({
      root,
      embedding: "test/deterministic",
      wait: true,
    });
    assert.equal(result.state, "failed");
    assert.equal(backend.modelPool.snapshot().activeLeases, 0);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("index preserves Qwen model creation diagnostics when the API key is missing", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-backend-qwen-missing-key-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const backend = new DaemonBackend({
    version: "1.0.0",
    serviceOptions: { apiKey: "" },
    watchManagerFactory: noopWatchManagerFactory,
  });

  try {
    const result = await backend.index({
      root,
      embedding: "qwen/text-embedding-v4",
      wait: true,
    });

    assert.equal(result.state, "failed");
    assert.equal(
      result.error.code,
      "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_V4_MISSING_API_KEY",
    );
    assert.equal(
      result.error.message,
      "Qwen text-embedding-v4 model requires an API key",
    );
    assert.match(result.error.context, /model=qwen\/text-embedding-v4/);
    assert.match(result.error.context, /hint=Pass --api-key/);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("index wraps unstructured model creation failures", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-backend-model-create-failure-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: {
      createModel: () => {
        throw new Error("fixture model creation failure");
      },
    },
    watchManagerFactory: noopWatchManagerFactory,
  });

  try {
    const result = await backend.index({
      root,
      embedding: "test/deterministic",
      wait: true,
    });

    assert.equal(result.state, "failed");
    assert.deepEqual(result.error, {
      code: "MODEL_LOAD_FAILED",
      message:
        "[MODEL_LOAD_FAILED] Embedding model test/deterministic could not be created: fixture model creation failure",
    });
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("index returns scan diagnostics only when debug is requested", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-backend-debug-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  await writeFile(join(root, "README.md"), "# Searchable\n");
  await writeFile(join(root, "large.ts"), "x".repeat(1024 * 1024 + 1));
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    createService: (options) =>
      createZvecGrep({
        ...options,
        embeddingModel: new TestEmbeddingModel(),
      }),
    watchManagerFactory: noopWatchManagerFactory,
  });

  try {
    const debug = await backend.index({
      root,
      embedding: "test/deterministic",
      wait: true,
      debug: true,
    });
    assert.equal(debug.state, "succeeded");
    assert.equal(debug.scanDiagnostics.skippedFiles, 1);
    assert.equal(debug.scanDiagnostics.skippedByReason.too_large, 1);
    assert.equal(
      debug.scanDiagnostics.skippedSamples[0].relativePath,
      "large.ts",
    );

    const normal = await backend.index({ root, wait: true });
    assert.equal(normal.state, "succeeded");
    assert.equal(normal.scanDiagnostics, undefined);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("drop index closes the active runtime and removes the persisted index", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-drop-backend-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  await writeFile(join(root, "answer.ts"), "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();

  let watcherCloses = 0;
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    createService: (options) =>
      createZvecGrep({
        ...options,
        embeddingModel: new TestEmbeddingModel(),
      }),
    watchManagerFactory: () => ({
      start() {},
      flushPending: async () => {},
      close: async () => {
        watcherCloses += 1;
      },
    }),
  });
  try {
    await backend.search(searchInput(root, "answer", "wait_for_fresh"));
    assert.equal((await backend.serverStatus()).activeRuntimes, 1);

    const result = await backend.dropIndex({ root });
    assert.deepEqual(result, { root: await realpath(root), removed: true });
    assert.equal(watcherCloses, 1);
    assert.equal((await backend.serverStatus()).activeRuntimes, 0);
    assert.equal((await backend.indexStatus({ root })).indexed, false);

    const repeated = await backend.dropIndex({ root });
    assert.deepEqual(repeated, { root: await realpath(root), removed: false });
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("drop index cancels an active indexing job before removing files", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-drop-cancel-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const canonicalRoot = await realpath(root);
  let jobSignal;
  let markJobStarted;
  const jobStarted = new Promise((resolve) => {
    markJobStarted = resolve;
  });
  let serviceClosed = false;
  const backend = new DaemonBackend({
    version: "1.0.0",
    createService: async () => ({
      dropIndex: async () => true,
      close: async () => {
        serviceClosed = true;
      },
    }),
  });
  try {
    backend.scheduler.submit({
      canonicalRoot,
      reason: "manual",
      run: (_report, signal) => {
        jobSignal = signal;
        markJobStarted();
        return new Promise((resolve) => {
          signal.addEventListener("abort", resolve, { once: true });
        });
      },
    });
    await jobStarted;

    const result = await backend.dropIndex({ root });

    assert.equal(jobSignal.aborted, true);
    assert.deepEqual(result, { root: canonicalRoot, removed: true });
    assert.equal(serviceClosed, true);
    assert.equal(backend.scheduler.getByRoot(canonicalRoot).state, "cancelled");
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("drop index cancels an active backend index job before dropping", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-drop-cancel-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  let markIndexStarted;
  const indexStarted = new Promise((resolve) => {
    markIndexStarted = resolve;
  });
  let indexSignal;
  let dropCalled = false;
  const backend = new DaemonBackend({
    version: "1.0.0",
    watchManagerFactory: noopWatchManagerFactory,
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    createService: async () => ({
      index: async (options) => {
        indexSignal = options.signal;
        markIndexStarted();
        await new Promise((resolve, reject) => {
          if (options.signal?.aborted) {
            reject(options.signal.reason);
            return;
          }
          options.signal?.addEventListener(
            "abort",
            () => reject(options.signal.reason),
            { once: true },
          );
        });
      },
      dropIndex: async () => {
        dropCalled = true;
        return true;
      },
      close: async () => {},
    }),
  });
  try {
    const index = await backend.index({
      root,
      embedding: "test/deterministic",
      wait: false,
    });
    await indexStarted;

    const drop = await backend.dropIndex({ root });
    const job = backend.scheduler.get(index.jobId);

    assert.equal(indexSignal.aborted, true);
    assert.equal(job.state, "cancelled");
    assert.equal(job.error.code, "INDEX_CANCELLED");
    assert.equal(dropCalled, true);
    assert.deepEqual(drop, { root: await realpath(root), removed: true });
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("wait_for_fresh reports a failed reconciliation instead of returning stale results", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-freshness-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  await writeFile(join(root, "answer.ts"), "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  await writeFile(
    join(root, "answer.ts"),
    "export const changedAnswer = 43;\n",
  );
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    watchManagerFactory: noopWatchManagerFactory,
    createService: async () => {
      throw new Error("reconciliation failed");
    },
  });
  try {
    await assert.rejects(
      backend.search({
        root,
        queries: ["answer"],
        routes: [],
        freshness: "wait_for_fresh",
      }),
      /reconciliation failed/,
    );
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("wait_for_fresh skips reconciliation when the initial probe is fresh", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-probe-fresh-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  await writeFile(join(root, "answer.ts"), "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  const events = [];
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    watchManagerFactory: noopWatchManagerFactory,
    createService: async () => {
      throw new Error("fresh indexes must not reconcile");
    },
    logger: {
      event: (name, fields) => events.push({ name, fields }),
      flush: async () => {},
    },
  });
  try {
    const result = await backend.search(
      searchInput(root, "answer", "wait_for_fresh"),
    );
    assert.equal(result.freshness, "fresh");
    assert.equal(result.indexing, undefined);
    assert.ok(
      events.some((event) => event.name === "runtime.initial_probe_fresh"),
    );
    assert.equal(backend.scheduler.getByRoot(await realpath(root)), undefined);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("eventual search can skip background reconciliation", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-no-update-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  await writeFile(join(root, "answer.ts"), "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  await writeFile(
    join(root, "answer.ts"),
    "export const changedAnswer = 43;\n",
  );
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    createService: async () => {
      throw new Error("background reconciliation must stay disabled");
    },
  });
  try {
    const result = await backend.search({
      ...searchInput(root, "answer", "eventual"),
      autoUpdate: false,
    });
    assert.equal(result.freshness, "possibly_stale");
    assert.deepEqual(result.indexing, {
      state: "idle",
      completed: 0,
      total: 1,
    });
    assert.equal(backend.scheduler.getByRoot(await realpath(root)), undefined);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("eventual search reports active indexing progress", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-search-progress-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  await writeFile(join(root, "answer.ts"), "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    watchManagerFactory: noopWatchManagerFactory,
  });
  let releaseIndexing = () => {};
  let jobId;
  try {
    await backend.search(searchInput(root, "answer", "wait_for_fresh"));
    const canonicalRoot = await realpath(root);
    let markIndexingStarted;
    const indexingStarted = new Promise((resolve) => {
      markIndexingStarted = resolve;
    });
    const indexingReleased = new Promise((resolve) => {
      releaseIndexing = resolve;
    });
    jobId = backend.scheduler.submit({
      canonicalRoot,
      reason: "watch",
      run: async (report) => {
        report({ phase: "indexing", filesIndexed: 4, filesTotal: 10 });
        markIndexingStarted();
        await indexingReleased;
      },
    }).job.id;
    await indexingStarted;

    const result = await backend.search({
      ...searchInput(root, "answer", "eventual"),
      autoUpdate: false,
    });
    assert.equal(result.freshness, "possibly_stale");
    assert.deepEqual(result.indexing, {
      state: "running",
      completed: 1,
      total: 1,
    });
  } finally {
    releaseIndexing();
    if (jobId) {
      await backend.scheduler.wait(jobId);
    }
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("eventual search queries while background reconciliation is running", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-background-query-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new QwenTestEmbeddingModel(),
  });
  await service.index();
  await service.close();

  let blockBackgroundEmbedding = false;
  let markBackgroundStarted;
  let releaseBackground = () => {};
  const backgroundStarted = new Promise((resolve) => {
    markBackgroundStarted = resolve;
  });
  const backgroundReleased = new Promise((resolve) => {
    releaseBackground = resolve;
  });
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: {
      createModel: () =>
        new QwenTestEmbeddingModel(async (contents) => {
          if (
            blockBackgroundEmbedding &&
            contents.some(
              (content) =>
                content.kind === "text" &&
                content.text.includes("changedAnswer"),
            )
          ) {
            markBackgroundStarted();
            await backgroundReleased;
          }
        }),
    },
    watchManagerFactory: noopWatchManagerFactory,
  });
  try {
    await writeFile(source, "export const changedAnswer = 43;\n");
    blockBackgroundEmbedding = true;

    const stale = await backend.search(searchInput(root, "answer", "eventual"));
    assert.equal(stale.freshness, "possibly_stale");
    assert.deepEqual(stale.indexing, {
      state: "running",
      completed: 0,
      total: 1,
    });
    await backgroundStarted;
    const updatingStatus = await backend.indexStatus({ root });
    assert.equal(updatingStatus.persistent.files.scanned, 1);
    assert.deepEqual(updatingStatus.runtime.completion, {
      completed: stale.indexing.completed,
      total: stale.indexing.total,
    });

    const plan = await backend.planSearchAuthorization(
      searchInput(root, "answer", "eventual"),
    );
    assert.equal(plan.operation, "query_and_index");

    const duringReconcile = await backend.search(
      searchInput(root, "answer", "eventual"),
    );

    assert.equal(duringReconcile.freshness, "possibly_stale");
    assert.deepEqual(duringReconcile.indexing, {
      state: "running",
      completed: 0,
      total: 1,
    });
  } finally {
    blockBackgroundEmbedding = false;
    releaseBackground();
    await backend.scheduler.waitForRootIdle(await realpath(root));
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("search requests reuse active runtime metadata without rescanning", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-authorization-cache-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  await writeFile(join(root, "answer.ts"), "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new QwenTestEmbeddingModel(),
  });
  await service.index();
  await service.close();

  const inspections = [];
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new QwenTestEmbeddingModel() },
    watchManagerFactory: noopWatchManagerFactory,
    inspectRoot: async (...args) => {
      inspections.push(args[2] ?? true);
      return await inspectRoot(...args);
    },
  });
  try {
    const first = await backend.planSearchAuthorization(
      searchInput(root, "answer", "eventual"),
    );
    await backend.search(searchInput(root, "answer", "eventual"));
    const activationInspectionCount = inspections.length;

    const second = await backend.planSearchAuthorization(
      searchInput(root, "answer", "eventual"),
    );
    await backend.search(searchInput(root, "answer", "eventual"));

    assert.equal(first.operation, "query");
    assert.equal(second.operation, "query");
    assert.ok(activationInspectionCount > 0);
    assert.equal(inspections.length, activationInspectionCount);

    await backend.runtimeManager.evict(await realpath(root));
    const afterEviction = await backend.planSearchAuthorization(
      searchInput(root, "answer", "eventual"),
    );
    assert.equal(afterEviction.operation, "query");
    assert.ok(inspections.length > activationInspectionCount);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("local search authorization discovers a parent index without a status scan", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-local-authorization-"),
  );
  const root = join(temporaryDirectory, "repo");
  const documents = join(root, "documents");
  await mkdir(documents, { recursive: true });
  await writeFile(join(documents, "answer.ts"), "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();

  const inspections = [];
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    watchManagerFactory: noopWatchManagerFactory,
    inspectRoot: async (...args) => {
      inspections.push(args[2] ?? true);
      return await inspectRoot(...args);
    },
  });
  try {
    const plan = await backend.planSearchAuthorization(
      searchInput(documents, "answer", "eventual"),
    );
    assert.equal(plan, undefined);
    assert.deepEqual(inspections, [false]);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("automatic remote watcher authorization reads metadata without a status scan", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-remote-watch-authorization-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  await writeFile(join(root, "answer.ts"), "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new QwenTestEmbeddingModel(),
  });
  await service.index();
  await service.close();

  const inspections = [];
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new QwenTestEmbeddingModel() },
    watchManagerFactory: noopWatchManagerFactory,
    inspectRoot: async (...args) => {
      inspections.push(args[2] ?? true);
      return await inspectRoot(...args);
    },
  });
  try {
    const runtime = await backend.runtimeManager.activate(root);
    const authorization = await backend.automaticIndexAuthorization(runtime);

    assert.equal(authorization.allowed, false);
    assert.deepEqual(inspections, [false]);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("wait_for_fresh consumes a running watch job without a status scan or full reconciliation", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-fresh-followup-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  let watcherOptions;
  let markWatchStarted;
  let releaseWatch = () => {};
  const watchStarted = new Promise((resolve) => {
    markWatchStarted = resolve;
  });
  const watchReleased = new Promise((resolve) => {
    releaseWatch = resolve;
  });
  let blockWatchEmbedding = false;
  const statusInspections = [];
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: {
      createModel: () =>
        new TestEmbeddingModel(async (contents) => {
          if (
            blockWatchEmbedding &&
            contents.some(
              (content) =>
                content.kind === "text" &&
                content.text.includes("changedAnswer"),
            )
          ) {
            markWatchStarted();
            await watchReleased;
          }
        }),
    },
    createService: async (options) => {
      const created = await createZvecGrep(options);
      return {
        ...created,
        root: created.root,
        index: (indexOptions) => created.index(indexOptions),
        disableIndex: (infoOptions) => created.disableIndex(infoOptions),
        info: (infoOptions) => created.info(infoOptions),
        context: (contextOptions) => created.context(contextOptions),
        close: () => created.close(),
      };
    },
    inspectRoot: async (...args) => {
      statusInspections.push(args[2] ?? true);
      return await inspectRoot(...args);
    },
    watchManagerFactory: (options) => {
      watcherOptions = options;
      return {
        start() {},
        flushPending: async () => {},
        close: async () => {},
      };
    },
  });
  try {
    await backend.search(searchInput(root, "answer", "eventual"));
    statusInspections.length = 0;
    const canonicalRoot = await realpath(root);
    await writeFile(source, "export const changedAnswer = 43;\n");
    blockWatchEmbedding = true;
    await watcherOptions.onChanges(
      {
        touchedFiles: [source],
        rescanDirectories: [],
        deletedPrefixes: [],
        forceFullReconcile: false,
      },
      "watch",
    );
    await watchStarted;
    const watchJob = backend.scheduler.getByRoot(canonicalRoot);
    assert.equal(watchJob.reason, "watch");
    const eventualResult = await backend.search({
      ...searchInput(root, "answer", "eventual"),
      autoUpdate: true,
    });
    assert.equal(eventualResult.freshness, "possibly_stale");
    assert.deepEqual(eventualResult.indexing, {
      state: "running",
      completed: 0,
      total: 1,
    });
    assert.equal(backend.scheduler.getByRoot(canonicalRoot).id, watchJob.id);
    let searchSettled = false;
    const search = backend
      .search({
        ...searchInput(root, "changedAnswer", "wait_for_fresh"),
        queries: undefined,
        routes: [{ mode: "fts", query: "changedAnswer" }],
      })
      .then((result) => {
        searchSettled = true;
        return result;
      });
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(searchSettled, false);
    blockWatchEmbedding = false;
    releaseWatch();

    const result = await search;
    assert.equal(result.freshness, "fresh");
    assert.match(result.result.items[0].content, /changedAnswer/);
    assert.equal(backend.scheduler.getByRoot(canonicalRoot).id, watchJob.id);
    assert.equal(backend.scheduler.getByRoot(canonicalRoot).reason, "watch");
    assert.deepEqual(statusInspections, [false, false]);
  } finally {
    releaseWatch();
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("eventual searches do not wait for full watcher writer preparation", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-eventual-writer-preparation-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();

  let watcherOptions;
  let blockWriterPreparation = false;
  let blockWatchEmbedding = false;
  let markWriterPreparationStarted;
  let releaseWriterPreparation = () => {};
  let markWatchEmbeddingStarted;
  let releaseWatchEmbedding = () => {};
  const writerPreparationStarted = new Promise((resolve) => {
    markWriterPreparationStarted = resolve;
  });
  const writerPreparationReleased = new Promise((resolve) => {
    releaseWriterPreparation = resolve;
  });
  const watchEmbeddingStarted = new Promise((resolve) => {
    markWatchEmbeddingStarted = resolve;
  });
  const watchEmbeddingReleased = new Promise((resolve) => {
    releaseWatchEmbedding = resolve;
  });
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: {
      createModel: () =>
        new TestEmbeddingModel(async (contents) => {
          if (
            blockWatchEmbedding &&
            contents.some(
              (content) =>
                content.kind === "text" &&
                content.text.includes("changedAnswer"),
            )
          ) {
            markWatchEmbeddingStarted();
            await watchEmbeddingReleased;
          }
        }),
    },
    createService: async (options) => {
      if (blockWriterPreparation) {
        markWriterPreparationStarted();
        await writerPreparationReleased;
      }
      return await createZvecGrep(options);
    },
    watchManagerFactory: (options) => {
      watcherOptions = options;
      return {
        start() {},
        flushPending: async () => {},
        close: async () => {},
      };
    },
  });

  try {
    await backend.search(searchInput(root, "answer", "wait_for_fresh"));
    await writeFile(source, "export const changedAnswer = 43;\n");
    blockWriterPreparation = true;
    blockWatchEmbedding = true;
    await watcherOptions.onChanges(
      {
        touchedFiles: [source],
        rescanDirectories: [],
        deletedPrefixes: [],
        forceFullReconcile: true,
      },
      "watch",
    );
    await writerPreparationStarted;

    let backgroundSettled = false;
    let offSettled = false;
    let waitSettled = false;
    const backgroundSearch = backend
      .search(searchInput(root, "answer", "eventual"))
      .then((result) => {
        backgroundSettled = true;
        return result;
      });
    const offSearch = backend
      .search({
        ...searchInput(root, "answer", "eventual"),
        autoUpdate: false,
      })
      .then((result) => {
        offSettled = true;
        return result;
      });
    const waitSearch = backend
      .search(searchInput(root, "changedAnswer", "wait_for_fresh"))
      .then((result) => {
        waitSettled = true;
        return result;
      });
    await Promise.race([
      Promise.all([backgroundSearch, offSearch]),
      new Promise((resolve) => setTimeout(resolve, 2_000)),
    ]);
    const settledDuringPreparation = {
      background: backgroundSettled,
      off: offSettled,
      wait: waitSettled,
    };

    blockWriterPreparation = false;
    releaseWriterPreparation();
    await watchEmbeddingStarted;
    const [backgroundResult, offResult] = await Promise.all([
      backgroundSearch,
      offSearch,
    ]);
    assert.equal(waitSettled, false);
    blockWatchEmbedding = false;
    releaseWatchEmbedding();
    const waitResult = await waitSearch;

    assert.deepEqual(settledDuringPreparation, {
      background: true,
      off: true,
      wait: false,
    });
    assert.equal(backgroundResult.freshness, "possibly_stale");
    assert.equal(offResult.freshness, "possibly_stale");
    assert.equal(waitResult.freshness, "fresh");
  } finally {
    releaseWriterPreparation();
    releaseWatchEmbedding();
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("routine watcher reconciliation stays fresh until its probe finds evidence", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-background-reconciliation-probe-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();

  let watcherOptions;
  let blockProbe = false;
  let markProbeStarted;
  let releaseProbe = () => {};
  const probeStarted = new Promise((resolve) => {
    markProbeStarted = resolve;
  });
  const probeReleased = new Promise((resolve) => {
    releaseProbe = resolve;
  });
  let blockBackgroundEmbedding = false;
  let markBackgroundStarted;
  let releaseBackground = () => {};
  const backgroundStarted = new Promise((resolve) => {
    markBackgroundStarted = resolve;
  });
  const backgroundReleased = new Promise((resolve) => {
    releaseBackground = resolve;
  });
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: {
      createModel: () =>
        new TestEmbeddingModel(async (contents) => {
          if (
            blockBackgroundEmbedding &&
            contents.some(
              (content) =>
                content.kind === "text" &&
                content.text.includes("changedAnswer"),
            )
          ) {
            markBackgroundStarted();
            await backgroundReleased;
          }
        }),
    },
    inspectRoot: async (...args) => {
      if (blockProbe && (args[2] ?? true)) {
        markProbeStarted();
        await probeReleased;
      }
      return await inspectRoot(...args);
    },
    watchManagerFactory: (options) => {
      watcherOptions = options;
      return {
        start() {},
        flushPending: async () => {},
        close: async () => {},
      };
    },
  });

  try {
    await backend.search(searchInput(root, "answer", "wait_for_fresh"));
    await watcherOptions.onChanges(
      {
        touchedFiles: [],
        rescanDirectories: [],
        deletedPrefixes: [],
        forceFullReconcile: true,
      },
      "reconcile",
    );

    const offResult = await backend.search({
      ...searchInput(root, "answer", "eventual"),
      autoUpdate: false,
    });
    assert.equal(offResult.freshness, "fresh");
    assert.equal(backend.scheduler.getByRoot(await realpath(root)), undefined);

    blockProbe = true;
    let backgroundSettled = false;
    const backgroundSearch = backend
      .search(searchInput(root, "answer", "eventual"))
      .then((result) => {
        backgroundSettled = true;
        return result;
      });
    await probeStarted;
    await Promise.race([
      backgroundSearch,
      new Promise((resolve) => setTimeout(resolve, 2_000)),
    ]);
    assert.equal(backgroundSettled, true);
    const backgroundResult = await backgroundSearch;
    assert.equal(backgroundResult.freshness, "fresh");

    let waitSettled = false;
    const waitSearch = backend
      .search(searchInput(root, "answer", "wait_for_fresh"))
      .then((result) => {
        waitSettled = true;
        return result;
      });
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(waitSettled, false);
    blockProbe = false;
    releaseProbe();
    assert.equal((await waitSearch).freshness, "fresh");

    await writeFile(source, "export const changedAnswer = 43;\n");
    await watcherOptions.onChanges(
      {
        touchedFiles: [],
        rescanDirectories: [],
        deletedPrefixes: [],
        forceFullReconcile: true,
      },
      "reconcile",
    );
    blockBackgroundEmbedding = true;
    await backend.search(searchInput(root, "answer", "eventual"));
    await backgroundStarted;

    const evidenceResult = await backend.search({
      ...searchInput(root, "answer", "eventual"),
      autoUpdate: false,
    });
    assert.equal(evidenceResult.freshness, "possibly_stale");
  } finally {
    releaseProbe();
    blockBackgroundEmbedding = false;
    releaseBackground();
    await backend.scheduler.waitForRootIdle(await realpath(root));
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("full watcher reconciliation skips the initial status scan", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watcher-full-scan-count-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();

  let watcherOptions;
  let recordScans = false;
  const includeStatusCalls = [];
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    inspectRoot: async (...args) => {
      if (recordScans) includeStatusCalls.push(args[2] ?? true);
      return await inspectRoot(...args);
    },
    watchManagerFactory: (options) => {
      watcherOptions = options;
      return {
        start() {},
        flushPending: async () => {},
        close: async () => {},
      };
    },
  });

  try {
    await backend.search(searchInput(root, "answer", "wait_for_fresh"));
    await writeFile(source, "export const changedAnswer = 43;\n");
    recordScans = true;
    await watcherOptions.onChanges(
      {
        touchedFiles: [source],
        rescanDirectories: [],
        deletedPrefixes: [],
        forceFullReconcile: true,
      },
      "reconcile",
    );
    await backend.search(searchInput(root, "changedAnswer", "wait_for_fresh"));

    assert.deepEqual(includeStatusCalls, [false, true]);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("wait_for_fresh flushes watcher changes until revisions are caught up", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-fresh-pending-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  let flushes = 0;
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    watchManagerFactory: (options) => {
      return {
        start() {},
        flushPending: async () => {
          flushes += 1;
          if (flushes === 1) {
            options.onPendingChange(true);
            return;
          }
          if (flushes === 2) {
            await options.onChanges(
              {
                touchedFiles: [source],
                rescanDirectories: [],
                deletedPrefixes: [],
                forceFullReconcile: false,
              },
              "watch",
            );
            options.onPendingChange(false);
          }
        },
        close: async () => {},
      };
    },
  });
  try {
    await backend.search(searchInput(root, "answer", "eventual"));
    await writeFile(source, "export const changedAnswer = 43;\n");

    const result = await backend.search({
      ...searchInput(root, "changedAnswer", "wait_for_fresh"),
      queries: undefined,
      routes: [{ mode: "fts", query: "changedAnswer" }],
    });

    assert.equal(flushes, 2);
    assert.equal(result.freshness, "fresh");
    assert.match(result.result.items[0].content, /changedAnswer/);
    const status = await backend.indexStatus({ root });
    assert.equal(status.runtime.dirtyRevision, status.runtime.indexedRevision);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("wait_for_fresh repeats a search when watcher changes arrive during it", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-fresh-during-search-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  let watcherOptions;
  let queuedChange = false;
  let blockQuery = false;
  let queryEmbeddings = 0;
  let markQueryStarted;
  let releaseQuery = () => {};
  const queryStarted = new Promise((resolve) => {
    markQueryStarted = resolve;
  });
  const queryReleased = new Promise((resolve) => {
    releaseQuery = resolve;
  });
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: {
      createModel: () =>
        new TestEmbeddingModel(async (contents) => {
          const embedsQuery = contents.some(
            (content) => content.kind === "text" && content.text === "answer",
          );
          if (!embedsQuery) {
            return;
          }
          queryEmbeddings += 1;
          if (blockQuery) {
            markQueryStarted();
            await queryReleased;
          }
        }),
    },
    watchManagerFactory: (options) => {
      watcherOptions = options;
      return {
        start() {},
        flushPending: async () => {
          if (!queuedChange) {
            return;
          }
          queuedChange = false;
          await options.onChanges(
            {
              touchedFiles: [source],
              rescanDirectories: [],
              deletedPrefixes: [],
              forceFullReconcile: false,
            },
            "watch",
          );
          options.onPendingChange(false);
        },
        close: async () => {},
      };
    },
  });
  try {
    await backend.search(searchInput(root, "answer", "wait_for_fresh"));
    const initialQueryEmbeddings = queryEmbeddings;
    blockQuery = true;
    const search = backend.search(
      searchInput(root, "answer", "wait_for_fresh"),
    );
    await queryStarted;
    await writeFile(source, "export const changedAnswer = 43;\n");
    queuedChange = true;
    watcherOptions.onPendingChange(true);
    blockQuery = false;
    releaseQuery();

    const result = await search;

    assert.equal(result.freshness, "fresh");
    assert.ok(queryEmbeddings >= initialQueryEmbeddings + 2);
    assert.match(result.result.items[0].content, /changedAnswer/);
  } finally {
    releaseQuery();
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("known watcher changes do not clear unknown initial drift", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-fresh-unknown-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  await writeFile(source, "export const changedAnswer = 43;\n");
  const scopes = [];
  let flushed = false;
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    createService: async (options) => {
      const created = await createZvecGrep(options);
      return {
        ...created,
        root: created.root,
        index: async (indexOptions) => {
          scopes.push(indexOptions.changedPaths ? "paths" : "full");
          return created.index(indexOptions);
        },
        disableIndex: (infoOptions) => created.disableIndex(infoOptions),
        info: (infoOptions) => created.info(infoOptions),
        context: (contextOptions) => created.context(contextOptions),
        close: () => created.close(),
      };
    },
    watchManagerFactory: (options) => ({
      start() {},
      flushPending: async () => {
        if (flushed) {
          return;
        }
        flushed = true;
        await options.onChanges(
          {
            touchedFiles: [source],
            rescanDirectories: [],
            deletedPrefixes: [],
            forceFullReconcile: false,
          },
          "watch",
        );
      },
      close: async () => {},
    }),
  });
  try {
    const result = await backend.search({
      ...searchInput(root, "changedAnswer", "wait_for_fresh"),
      queries: undefined,
      routes: [{ mode: "fts", query: "changedAnswer" }],
    });

    assert.deepEqual(scopes, ["paths", "full"]);
    assert.equal(result.freshness, "fresh");
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("unknown drift during a manual job survives until a full follow-up", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-fresh-deferred-full-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  let watcherOptions;
  let blockManual = false;
  let markManualIndexed;
  let releaseManual = () => {};
  const manualIndexed = new Promise((resolve) => {
    markManualIndexed = resolve;
  });
  const manualReleased = new Promise((resolve) => {
    releaseManual = resolve;
  });
  const scopes = [];
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    createService: async (options) => {
      const created = await createZvecGrep(options);
      return {
        ...created,
        root: created.root,
        index: async (indexOptions) => {
          scopes.push(indexOptions.changedPaths ? "paths" : "full");
          const result = await created.index(indexOptions);
          if (blockManual && !indexOptions.changedPaths) {
            markManualIndexed();
            await manualReleased;
          }
          return result;
        },
        disableIndex: (infoOptions) => created.disableIndex(infoOptions),
        info: (infoOptions) => created.info(infoOptions),
        context: (contextOptions) => created.context(contextOptions),
        close: () => created.close(),
      };
    },
    watchManagerFactory: (options) => {
      watcherOptions = options;
      return {
        start() {},
        flushPending: async () => {},
        close: async () => {},
      };
    },
  });
  try {
    await backend.search(searchInput(root, "answer", "wait_for_fresh"));
    blockManual = true;
    const manual = backend.index({ root, wait: true });
    await manualIndexed;
    await writeFile(source, "export const changedAnswer = 43;\n");
    watcherOptions.onPendingChange(true);
    await watcherOptions.onChanges(
      {
        touchedFiles: [],
        rescanDirectories: [],
        deletedPrefixes: [],
        forceFullReconcile: true,
      },
      "reconcile",
    );
    watcherOptions.onPendingChange(false);
    blockManual = false;
    releaseManual();
    assert.equal((await manual).state, "succeeded");

    const result = await backend.search({
      ...searchInput(root, "changedAnswer", "wait_for_fresh"),
      queries: undefined,
      routes: [{ mode: "fts", query: "changedAnswer" }],
    });

    assert.deepEqual(scopes, ["full", "full"]);
    assert.equal(result.freshness, "fresh");
    assert.match(result.result.items[0].content, /changedAnswer/);
  } finally {
    releaseManual();
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("unknown drift after a full proof is preserved for a follow-up", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-fresh-after-proof-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  let watcherOptions;
  let injectAfterProof = false;
  const scopes = [];
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    createService: async (options) => {
      const created = await createZvecGrep(options);
      return {
        ...created,
        root: created.root,
        index: async (indexOptions) => {
          scopes.push(indexOptions.changedPaths ? "paths" : "full");
          return created.index(indexOptions);
        },
        disableIndex: (infoOptions) => created.disableIndex(infoOptions),
        info: (infoOptions) => created.info(infoOptions),
        context: (contextOptions) => created.context(contextOptions),
        close: () => created.close(),
      };
    },
    watchManagerFactory: (options) => {
      watcherOptions = options;
      return {
        start() {},
        flushPending: async () => {},
        close: async () => {},
      };
    },
    logger: {
      event: (name) => {
        if (name !== "index.completed" || !injectAfterProof) {
          return;
        }
        injectAfterProof = false;
        writeFileSync(source, "export const changedAnswer = 43;\n");
        watcherOptions.onPendingChange(true);
        watcherOptions.onChanges(
          {
            touchedFiles: [],
            rescanDirectories: [],
            deletedPrefixes: [],
            forceFullReconcile: true,
          },
          "reconcile",
        );
        watcherOptions.onPendingChange(false);
      },
      flush: async () => {},
    },
  });
  try {
    await backend.search(searchInput(root, "answer", "wait_for_fresh"));
    injectAfterProof = true;
    assert.equal(
      (await backend.index({ root, wait: true })).state,
      "succeeded",
    );

    const result = await backend.search({
      ...searchInput(root, "changedAnswer", "wait_for_fresh"),
      queries: undefined,
      routes: [{ mode: "fts", query: "changedAnswer" }],
    });

    assert.deepEqual(scopes, ["full", "full"]);
    assert.equal(result.freshness, "fresh");
    assert.match(result.result.items[0].content, /changedAnswer/);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("wait_for_fresh does not replace a failed path update with a full scan", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-fresh-path-failure-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  let watcherOptions;
  const scopes = [];
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    createService: async (options) => {
      const created = await createZvecGrep(options);
      return {
        ...created,
        root: created.root,
        index: async (indexOptions) => {
          scopes.push(indexOptions.changedPaths ? "paths" : "full");
          if (indexOptions.changedPaths) {
            throw new Error("path update failed");
          }
          return created.index(indexOptions);
        },
        disableIndex: (infoOptions) => created.disableIndex(infoOptions),
        info: (infoOptions) => created.info(infoOptions),
        context: (contextOptions) => created.context(contextOptions),
        close: () => created.close(),
      };
    },
    watchManagerFactory: (options) => {
      watcherOptions = options;
      return {
        start() {},
        flushPending: async () => {},
        close: async () => {},
      };
    },
  });
  try {
    await backend.search(searchInput(root, "answer", "wait_for_fresh"));
    await writeFile(source, "export const changedAnswer = 43;\n");
    await watcherOptions.onChanges(
      {
        touchedFiles: [source],
        rescanDirectories: [],
        deletedPrefixes: [],
        forceFullReconcile: false,
      },
      "watch",
    );
    const canonicalRoot = await realpath(root);
    await backend.scheduler.waitForRootIdle(canonicalRoot);
    const failedJob = backend.scheduler.getByRoot(canonicalRoot);

    const eventual = await backend.search({
      ...searchInput(root, "answer", "eventual"),
      autoUpdate: true,
    });
    assert.equal(eventual.freshness, "possibly_stale");
    assert.equal(eventual.indexing.state, "failed");
    assert.equal(backend.scheduler.getByRoot(canonicalRoot).id, failedJob.id);
    assert.deepEqual(scopes, ["paths"]);

    await assert.rejects(
      backend.search(searchInput(root, "answer", "wait_for_fresh")),
      /path update failed/,
    );
    assert.deepEqual(scopes, ["paths"]);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("wait_for_fresh probes before retrying a failed full reconciliation", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-fresh-recovery-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  let watcherOptions;
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    createService: async () => {
      throw new Error("path update failed");
    },
    watchManagerFactory: (options) => {
      watcherOptions = options;
      return {
        start() {},
        flushPending: async () => {},
        close: async () => {},
      };
    },
  });
  try {
    await backend.search(searchInput(root, "answer", "wait_for_fresh"));
    await watcherOptions.onChanges(
      {
        touchedFiles: [source],
        rescanDirectories: [],
        deletedPrefixes: [],
        forceFullReconcile: true,
      },
      "reconcile",
    );
    const canonicalRoot = await realpath(root);
    await backend.scheduler.waitForRootIdle(canonicalRoot);
    assert.equal(backend.scheduler.getByRoot(canonicalRoot).state, "failed");

    const result = await backend.search(
      searchInput(root, "answer", "wait_for_fresh"),
    );

    assert.equal(result.freshness, "fresh");
    assert.equal(
      backend.scheduler.getByRoot(canonicalRoot).reason,
      "reconcile",
    );
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("concurrent backend close calls wait for the same shutdown drain", async () => {
  const backend = new DaemonBackend({ version: "1.0.0" });
  let release;
  backend.scheduler.submit({
    canonicalRoot: "/repo",
    reason: "manual",
    run: () =>
      new Promise((resolve) => {
        release = resolve;
      }),
  });
  while (!release) {
    await new Promise((resolve) => setImmediate(resolve));
  }
  let secondClosed = false;
  const first = backend.close();
  const second = backend.close().then(() => {
    secondClosed = true;
  });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(secondClosed, false);
  release();
  await Promise.all([first, second]);
  assert.equal(secondClosed, true);
});

test("backend close cancels active indexing jobs", async () => {
  const backend = new DaemonBackend({ version: "1.0.0" });
  let jobSignal;
  let markJobStarted;
  const jobStarted = new Promise((resolve) => {
    markJobStarted = resolve;
  });
  backend.scheduler.submit({
    canonicalRoot: "/repo",
    reason: "manual",
    run: (_report, signal) => {
      jobSignal = signal;
      markJobStarted();
      return new Promise((resolve) => {
        signal.addEventListener("abort", resolve, { once: true });
      });
    },
  });
  await jobStarted;

  await backend.close();

  assert.equal(jobSignal.aborted, true);
  const job = backend.scheduler.getByRoot("/repo");
  assert.equal(job.state, "cancelled");
  assert.equal(job.error.code, "INDEX_CANCELLED");
});

test("watch changes use the path-level index pipeline and advance revisions", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watch-backend-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const source = join(root, "answer.ts");
  await writeFile(source, "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  let watcherOptions;
  let watcherCloses = 0;
  const indexedPathBatches = [];
  let markPathIndexStarted;
  let releasePathIndex;
  const pathIndexStarted = new Promise((resolve) => {
    markPathIndexStarted = resolve;
  });
  const pathIndexReleased = new Promise((resolve) => {
    releasePathIndex = resolve;
  });
  const backend = new DaemonBackend({
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    runtimeIdleTtlMs: 1_000,
    createService: async (options) => {
      const created = await createZvecGrep(options);
      return {
        ...created,
        root: created.root,
        index: async (indexOptions) => {
          if (indexOptions.changedPaths) {
            indexedPathBatches.push([...indexOptions.changedPaths]);
            markPathIndexStarted();
            await pathIndexReleased;
          }
          return created.index(indexOptions);
        },
        disableIndex: (infoOptions) => created.disableIndex(infoOptions),
        info: (infoOptions) => created.info(infoOptions),
        context: (contextOptions) => created.context(contextOptions),
        close: () => created.close(),
      };
    },
    watchManagerFactory: (options) => {
      watcherOptions = options;
      return {
        start() {},
        flushPending: async () => {},
        close: async () => {
          watcherCloses += 1;
        },
      };
    },
  });
  try {
    await backend.search(searchInput(root, "answer", "wait_for_fresh"));
    await writeFile(source, "export const updatedAnswer = 43;\n");
    await watcherOptions.onChanges({
      touchedFiles: [source],
      rescanDirectories: [],
      deletedPrefixes: [],
      forceFullReconcile: false,
    });
    await pathIndexStarted;
    const manualIndex = backend.index({ root, wait: true });
    releasePathIndex();
    assert.equal((await manualIndex).state, "succeeded");
    const canonicalRoot = await realpath(root);
    await backend.scheduler.waitForRootIdle(canonicalRoot);
    const result = await backend.search({
      ...searchInput(root, "updatedAnswer", "eventual"),
      queries: undefined,
      routes: [{ mode: "fts", query: "updatedAnswer" }],
    });
    assert.match(result.result.items[0].content, /updatedAnswer/);
    assert.deepEqual(indexedPathBatches, [[source]]);
    const status = await backend.indexStatus({ root });
    assert.equal(status.runtime.watcherActive, true);
    assert.equal(status.runtime.dirtyRevision, 2);
    assert.equal(status.runtime.indexedRevision, 2);
    await waitFor(() => watcherCloses === 1);
    assert.equal((await backend.serverStatus()).activeRuntimes, 0);
  } finally {
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("daemon restart forgets runtimes and jobs but preserves index discovery", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-restart-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  await writeFile(join(root, "answer.ts"), "export const answer = 42;\n");
  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  await service.index();
  await service.close();
  const options = {
    version: "1.0.0",
    modelPoolOptions: { createModel: () => new TestEmbeddingModel() },
    watchManagerFactory: noopWatchManagerFactory,
  };
  const first = new DaemonBackend(options);
  try {
    await first.search(searchInput(root, "answer", "wait_for_fresh"));
    assert.equal((await first.serverStatus()).activeRuntimes, 1);
  } finally {
    await first.close();
  }

  const second = new DaemonBackend(options);
  try {
    const server = await second.serverStatus();
    assert.equal(server.activeRuntimes, 0);
    assert.equal(server.queuedJobs, 0);
    assert.equal(server.runningJobs, 0);
    const index = await second.indexStatus({ root });
    assert.equal(index.indexed, true);
    assert.equal(index.runtime, undefined);
  } finally {
    await second.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

class TestEmbeddingModel extends BaseEmbeddingModel {
  info = {
    reference: "test/deterministic",
    provider: "test",
    name: "deterministic",
    dimension: 8,
    metric: "cosine",
    inputKinds: ["text"],
    limits: { maxBatchSize: 64 },
  };

  constructor(beforeEmbed = async () => {}) {
    super();
    this.beforeEmbed = beforeEmbed;
  }

  async doEmbed(contents) {
    await this.beforeEmbed(contents);
    return {
      vectors: contents.map(() => [1, 0, 0, 0, 0, 0, 0, 0]),
      truncated: [],
    };
  }
}

class QwenTestEmbeddingModel extends TestEmbeddingModel {
  info = {
    reference: "qwen/text-embedding-v4",
    provider: "qwen",
    name: "text-embedding-v4",
    dimension: 8,
    metric: "cosine",
    endpoint: "https://qwen.test/embeddings",
    inputKinds: ["text"],
    limits: { maxBatchSize: 64 },
  };
}

function searchInput(root, query, freshness) {
  return {
    root,
    queries: [query],
    routes: [],
    freshness,
    autoUpdate: true,
  };
}

async function waitFor(predicate) {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error("Condition was not reached.");
}
