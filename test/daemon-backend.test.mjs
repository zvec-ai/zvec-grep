import assert from "node:assert/strict";
import { writeFileSync } from "node:fs";
import { mkdir, mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { DaemonBackend } from "../dist/daemon/backend.js";
import { EmbeddingModel } from "../dist/engine/models/embeddings.js";
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
      createModel: () => ({ dispose: async () => {} }),
    },
    resolveEmbeddingSchema: () => ({
      provider: "test",
      model: "deterministic",
      dimension: 8,
      metric: "cosine",
    }),
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
        maxContentChars: 1_200,
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
    assert.deepEqual(result.indexing, { state: "idle" });
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
      completed: 4,
      total: 10,
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

test("wait_for_fresh consumes a running watch job without a full reconciliation", async () => {
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
        collections: created.collections,
        index: (indexOptions) => created.index(indexOptions),
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
    await backend.search(searchInput(root, "answer", "eventual"));
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
  } finally {
    releaseWatch();
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
        collections: created.collections,
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
        collections: created.collections,
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
        collections: created.collections,
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
        collections: created.collections,
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
        collections: created.collections,
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

class TestEmbeddingModel extends EmbeddingModel {
  ref = { provider: "test", model: "deterministic" };
  dimension = 8;
  metric = "cosine";
  supportedContentKinds = ["text"];
  limits = { maxBatchSize: 64 };

  constructor(beforeEmbed = async () => {}) {
    super();
    this.beforeEmbed = beforeEmbed;
  }

  async doEmbed(contents) {
    await this.beforeEmbed(contents);
    return contents.map(() => [1, 0, 0, 0, 0, 0, 0, 0]);
  }
}

function searchInput(root, query, freshness) {
  return {
    root,
    queries: [query],
    routes: [],
    freshness,
    autoUpdate: true,
    maxContentChars: 1_200,
  };
}

async function waitFor(predicate) {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error("Condition was not reached.");
}
