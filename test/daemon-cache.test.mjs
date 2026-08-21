import assert from "node:assert/strict";
import test from "node:test";
import { EmbeddingModelPool } from "../dist/daemon/model-pool.js";
import { WorkspaceReadSessionCache } from "../dist/daemon/workspace-read-session-cache.js";
import { RootRuntime } from "../dist/daemon/root-runtime.js";
import { createZvecGrep } from "../dist/index.js";

test("workspace read session cache opens once, serializes operations and waits for readers before close", async () => {
  let opens = 0;
  let closes = 0;
  let activeOperations = 0;
  let maxActiveOperations = 0;
  let releaseFirst;
  const firstBlocked = new Promise((resolve) => {
    releaseFirst = resolve;
  });
  let markFirstStarted;
  const firstStarted = new Promise((resolve) => {
    markFirstStarted = resolve;
  });
  const cache = new WorkspaceReadSessionCache({
    open: async () => {
      opens += 1;
      return {
        close: async () => {
          closes += 1;
        },
      };
    },
    idleTtlMs: 60_000,
  });

  const first = cache.withRead(async () => {
    activeOperations += 1;
    maxActiveOperations = Math.max(maxActiveOperations, activeOperations);
    markFirstStarted();
    await firstBlocked;
    activeOperations -= 1;
    return "first";
  });
  await firstStarted;
  const second = cache.withRead(async () => {
    activeOperations += 1;
    maxActiveOperations = Math.max(maxActiveOperations, activeOperations);
    activeOperations -= 1;
    return "second";
  });
  const close = cache.close();
  await Promise.resolve();
  assert.equal(closes, 0);

  releaseFirst();
  assert.deepEqual(await Promise.all([first, second]), ["first", "second"]);
  await close;
  assert.equal(opens, 1);
  assert.equal(closes, 1);
  assert.equal(maxActiveOperations, 1);
});

test("embedding model pool single-flights loads and disposes after the final lease", async () => {
  let creates = 0;
  let disposals = 0;
  const model = {
    dispose: async () => {
      disposals += 1;
    },
  };
  const pool = new EmbeddingModelPool({
    idleTtlMs: 0,
    createModel: async () => {
      creates += 1;
      await Promise.resolve();
      return model;
    },
  });
  const request = {
    model: { provider: "local", name: "test" },
  };

  const [first, second] = await Promise.all([
    pool.acquire(request),
    pool.acquire(request),
  ]);
  assert.equal(creates, 1);
  assert.deepEqual(pool.snapshot(), { loaded: 1, activeLeases: 2 });
  first.release();
  assert.equal(disposals, 0);
  second.release();
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(disposals, 1);
  assert.deepEqual(pool.snapshot(), { loaded: 0, activeLeases: 0 });
  await pool.close();
});

test("model pool rolls back an unreturned lease when capacity trimming fails", async () => {
  const pool = new EmbeddingModelPool({
    idleTtlMs: 60_000,
    maxLoadedModels: 1,
    keyForRequest: (request) => request.model.name,
    createModel: (request) => ({
      dispose: async () => {
        if (request.model.name === "model-a") {
          throw new Error("dispose failed");
        }
      },
    }),
  });
  const first = await pool.acquire(modelLoadRequest("model-a"));
  first.release();
  await assert.rejects(
    pool.acquire(modelLoadRequest("model-b")),
    /dispose failed/,
  );
  assert.equal(pool.snapshot().activeLeases, 0);
  await pool.close();
});

test("model pool close drains an in-flight load without returning a lease", async () => {
  let finishLoad;
  let disposals = 0;
  const pool = new EmbeddingModelPool({
    createModel: () =>
      new Promise((resolve) => {
        finishLoad = () =>
          resolve({
            dispose: async () => {
              disposals += 1;
            },
          });
      }),
  });
  const acquiring = pool.acquire(modelLoadRequest("model-a"));
  while (!finishLoad) {
    await new Promise((resolve) => setImmediate(resolve));
  }
  const closing = pool.close();
  finishLoad();
  await assert.rejects(acquiring, /pool is closed/);
  await closing;
  assert.equal(disposals, 1);
  assert.deepEqual(pool.snapshot(), { loaded: 0, activeLeases: 0 });
});

test("service does not dispose a borrowed embedding model", async () => {
  let disposals = 0;
  const model = {
    dispose: async () => {
      disposals += 1;
    },
  };
  const service = await createZvecGrep({
    root: process.cwd(),
    embeddingModel: model,
    embeddingModelOwnership: "borrowed",
  });
  await service.close();
  assert.equal(disposals, 0);
});

test("root runtime releases model leases when the read session closes", async () => {
  let sessionCloses = 0;
  let modelDisposals = 0;
  const pool = new EmbeddingModelPool({
    idleTtlMs: 0,
    keyForRequest: (request) => request.model.name,
    createModel: () => ({
      dispose: async () => {
        modelDisposals += 1;
      },
    }),
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: modelLoadRequest("model-a"),
    readSessionIdleTtlMs: 0,
    openSession: async () => ({
      root: "/tmp/repo",
      context: async () => emptyContextResult(),
      close: async () => {
        sessionCloses += 1;
      },
    }),
  });

  await runtime.search({ query: "query" });
  await new Promise((resolve) => setImmediate(resolve));
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(sessionCloses, 1);
  assert.equal(modelDisposals, 1);
  assert.deepEqual(pool.snapshot(), { loaded: 0, activeLeases: 0 });
  await runtime.close();
  await pool.close();
});

test("root runtime graph reads use a model-free session cache", async () => {
  let modelLoads = 0;
  let graphOpens = 0;
  let graphCloses = 0;
  const pool = new EmbeddingModelPool({
    createModel: () => {
      modelLoads += 1;
      throw new Error("embedding model must not load");
    },
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    readSessionIdleTtlMs: 60_000,
    openGraphSession: async () => {
      graphOpens += 1;
      return {
        root: "/tmp/repo",
        explore: async (options) => ({
          root: "/tmp/repo",
          available: true,
          query: options.query,
          roots: [],
          nodes: [],
          edges: [],
          callPaths: [],
          blastRadius: [],
          changeSurface: [],
          files: [],
          emptyReason: "no_seeds",
        }),
        graphNeighborhood: async (options) => ({
          root: "/tmp/repo",
          available: true,
          direction: options.direction,
          query: options.query,
          depth: options.depth ?? 1,
          limit: options.limit ?? 20,
          seeds: [],
          neighbors: [],
        }),
        close: async () => {
          graphCloses += 1;
        },
      };
    },
  });

  await runtime.explore({ query: "login" });
  await runtime.graphNeighborhood({
    direction: "callers",
    query: "login",
  });

  assert.equal(modelLoads, 0);
  assert.deepEqual(pool.snapshot(), { loaded: 0, activeLeases: 0 });
  assert.equal(graphOpens, 1);
  await runtime.close();
  assert.equal(graphCloses, 1);
  await pool.close();
});

test("root runtime replaces a cached session when the embedding model changes", async () => {
  let modelLoads = 0;
  let sessionCloses = 0;
  const pool = new EmbeddingModelPool({
    idleTtlMs: 0,
    maxLoadedModels: 2,
    keyForRequest: (request) => request.model.name,
    createModel: () => {
      modelLoads += 1;
      return { dispose: async () => {} };
    },
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: modelLoadRequest("model-a"),
    readSessionIdleTtlMs: 60_000,
    openSession: async () => ({
      root: "/tmp/repo",
      context: async () => emptyContextResult(),
      close: async () => {
        sessionCloses += 1;
      },
    }),
  });

  await runtime.search({ query: "first" });
  runtime.updateModelLoadRequest(modelLoadRequest("model-b"));
  await runtime.search({ query: "second" });
  assert.equal(modelLoads, 2);
  assert.equal(sessionCloses, 1);
  await runtime.close();
  await pool.close();
});

test("root runtime searches the writer context as soon as it becomes available", async () => {
  const pool = new EmbeddingModelPool({
    createModel: () => ({ dispose: async () => {} }),
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: modelLoadRequest("model-a"),
  });

  runtime.setWriterPending(true);
  let searchSettled = false;
  const search = runtime.search({ query: "eventual" }).then((result) => {
    searchSettled = true;
    return result;
  });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(searchSettled, false);

  const releaseWriterContext = runtime.setWriterContext(
    async (options) => {
      assert.equal(options.root, "/tmp/repo");
      assert.equal(options.autoUpdate, false);
      return { ...emptyContextResult(), query: options.query };
    },
    pool.keyFor(modelLoadRequest("model-a")),
  );

  const result = await search;
  assert.equal(searchSettled, true);
  assert.equal(result.query, "eventual");

  await releaseWriterContext();
  runtime.setWriterPending(false);
  await runtime.close();
  await pool.close();
});

test("root runtime marks writer pending only while a write owns the runtime", async () => {
  const pool = new EmbeddingModelPool({
    createModel: () => ({ dispose: async () => {} }),
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: modelLoadRequest("model-a"),
  });
  let markWriteStarted;
  let releaseWrite;
  const writeStarted = new Promise((resolve) => {
    markWriteStarted = resolve;
  });
  const writeReleased = new Promise((resolve) => {
    releaseWrite = resolve;
  });

  const write = runtime.withWrite(async () => {
    markWriteStarted();
    await writeReleased;
  });
  await writeStarted;
  assert.equal(runtime.snapshot().writerPending, true);
  releaseWrite();
  await write;
  assert.equal(runtime.snapshot().writerPending, false);

  await runtime.close();
  await pool.close();
});

test("root runtime does not reuse a writer context with a different model runtime", async () => {
  const pool = new EmbeddingModelPool({
    keyForRequest: (request) => request.runtime.apiKey,
    createModel: () => ({ dispose: async () => {} }),
  });
  const writerRequest = {
    model: { provider: "qwen", name: "text-embedding-v4" },
    runtime: { apiKey: "writer-key" },
  };
  const searchRequest = {
    model: { provider: "qwen", name: "text-embedding-v4" },
    runtime: { apiKey: "search-key" },
  };
  let writerSearches = 0;
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: writerRequest,
    openSession: async (lease) => ({
      root: "/tmp/repo",
      context: async () => ({ ...emptyContextResult(), query: lease.key }),
      close: async () => {},
    }),
  });

  runtime.setWriterPending(true);
  const releaseWriterContext = runtime.setWriterContext(async () => {
    writerSearches += 1;
    return { ...emptyContextResult(), query: "writer-key" };
  }, pool.keyFor(writerRequest));
  runtime.updateModelLoadRequest(searchRequest);
  const search = runtime.search({ query: "query" }, searchRequest);
  await new Promise((resolve) => setImmediate(resolve));
  const writerSearchesBeforeRelease = writerSearches;

  await releaseWriterContext();
  runtime.setWriterPending(false);
  const result = await search;
  await runtime.close();
  await pool.close();

  assert.equal(writerSearchesBeforeRelease, 0);
  assert.equal(result.query, "search-key");
});

test("root runtime releases its daemon lease when read cache close fails", async () => {
  let releases = 0;
  const pool = new EmbeddingModelPool({
    createModel: () => ({ dispose: async () => {} }),
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: modelLoadRequest("model-a"),
    rootLease: {
      root: "/tmp/repo",
      release: async () => {
        releases += 1;
      },
    },
    readSessionIdleTtlMs: 60_000,
    openSession: async () => ({
      root: "/tmp/repo",
      context: async () => emptyContextResult(),
      close: async () => {
        throw new Error("session close failed");
      },
    }),
  });
  await runtime.search({ query: "query" });
  await assert.rejects(runtime.close(), /session close failed/);
  assert.equal(releases, 1);
  await pool.close();
});

test("root runtime initial probe marks a clean index reconciled", async () => {
  const pool = new EmbeddingModelPool({
    createModel: () => ({ dispose: async () => {} }),
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: modelLoadRequest("model-a"),
  });

  assert.equal(runtime.needsReconciliation(), true);
  assert.equal(await runtime.probeInitialFreshness(async () => true), "fresh");
  assert.equal(runtime.needsReconciliation(), false);
  await runtime.close();
  await pool.close();
});

test("root runtime initial probe does not hide pending watcher changes", async () => {
  const pool = new EmbeddingModelPool({
    createModel: () => ({ dispose: async () => {} }),
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: modelLoadRequest("model-a"),
  });
  let finishProbe;
  const probe = runtime.probeInitialFreshness(
    () =>
      new Promise((resolve) => {
        finishProbe = resolve;
      }),
  );
  runtime.setWatcherPending(true);
  finishProbe(true);

  assert.equal(await probe, "stale");
  assert.equal(runtime.needsReconciliation(), true);
  await runtime.close();
  await pool.close();
});

function modelLoadRequest(model) {
  return {
    model: { provider: "test", name: model },
  };
}

function emptyContextResult() {
  return {
    query: "query",
    root: "/tmp/repo",
    source: "index",
    coverage: "ranked_sample",
    diagnostics: {},
    items: [],
  };
}
