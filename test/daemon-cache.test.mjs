import assert from "node:assert/strict";
import test from "node:test";
import { EmbeddingModelPool } from "../dist/daemon/model-pool.js";
import { WorkspaceReadSessionCache } from "../dist/daemon/workspace-read-session-cache.js";
import { RootRuntime } from "../dist/daemon/root-runtime.js";
import { createZvecGrep } from "../dist/index.js";
import { CURRENT_INDEX_VERSION } from "../dist/engine/types.js";
import {
  preflightSearchPlan,
  searchWorkspaceIndex,
} from "../dist/engine/pipeline/search/index.js";
import { FakeEmbeddingModel } from "./helpers/fake-embedding.mjs";

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

test("root runtime releases the short query model lease while its storage session stays cached", async () => {
  let modelDisposals = 0;
  let queryEmbeds = 0;
  const storage = readFixture();
  const model = new FakeEmbeddingModel();
  const doEmbed = model.doEmbed.bind(model);
  model.doEmbed = async (contents, options) => {
    queryEmbeds += 1;
    assert.equal(options.purpose, "query");
    assert.equal(pool.snapshot().activeLeases, 1);
    assert.equal(runtime.snapshot().activeReaders, 0);
    return doEmbed(contents, options);
  };
  model.dispose = async () => {
    modelDisposals += 1;
  };
  const pool = new EmbeddingModelPool({
    idleTtlMs: 0,
    createModel: () => model,
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: modelLoadRequest("deterministic"),
    readSessionIdleTtlMs: 60_000,
    openSession: storage.openSession,
  });
  storage.beforeConsume = () => {
    assert.equal(pool.snapshot().activeLeases, 0);
  };
  try {
    await runtime.search({ query: "connection pool" });
    assert.equal(queryEmbeds, 1);
    assert.equal(modelDisposals, 1);
    assert.deepEqual(pool.snapshot(), { loaded: 0, activeLeases: 0 });
    assert.equal(storage.calls.opens, 1);
    assert.equal(storage.calls.closes, 0);
    assert.equal(runtime.snapshot().readSessionOpen, true);
    assert.ok(storage.calls.fts > 0);
    assert.ok(storage.calls.vector > 0);
  } finally {
    await runtime.close();
    await pool.close();
  }
  assert.equal(storage.calls.closes, 1);
});

test("watcher pending state does not count as runtime activity", async () => {
  const pool = new EmbeddingModelPool({
    createModel: () => ({ dispose: async () => {} }),
  });
  let activities = 0;
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    onActivity: () => {
      activities += 1;
    },
  });

  runtime.setWatcherPending(true);
  assert.equal(runtime.snapshot().watcherPending, true);
  runtime.setWatcherPending(false);
  assert.equal(runtime.snapshot().watcherPending, false);
  assert.equal(activities, 0);

  runtime.recordWatcherActivity();
  assert.equal(activities, 1);
  await runtime.close();
  await pool.close();
});

test("root runtime keeps the storage generation cached across same-schema model runtime changes", async () => {
  const modelLoads = [];
  const embeddings = [];
  const storage = readFixture();
  const pool = new EmbeddingModelPool({
    idleTtlMs: 0,
    maxLoadedModels: 2,
    keyForRequest: (request) => request.runtime.apiKey,
    createModel: (request) => {
      modelLoads.push(request.runtime.apiKey);
      const model = new FakeEmbeddingModel();
      const doEmbed = model.doEmbed.bind(model);
      model.doEmbed = async (contents, options) => {
        embeddings.push([
          request.runtime.apiKey,
          ...contents.map((c) => c.text),
        ]);
        return doEmbed(contents, options);
      };
      return model;
    },
  });
  const firstRequest = {
    ...modelLoadRequest("deterministic"),
    runtime: { apiKey: "first-key" },
  };
  const secondRequest = {
    ...modelLoadRequest("deterministic"),
    runtime: { apiKey: "second-key" },
  };
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: firstRequest,
    readSessionIdleTtlMs: 60_000,
    openSession: storage.openSession,
  });
  try {
    await runtime.search({ query: "first query" });
    runtime.updateModelLoadRequest(secondRequest);
    await runtime.search({ query: "second query" });
    assert.deepEqual(modelLoads, ["first-key", "second-key"]);
    assert.deepEqual(embeddings, [
      ["first-key", "first query"],
      ["second-key", "second query"],
    ]);
    assert.equal(storage.calls.opens, 1);
    assert.equal(storage.calls.closes, 0);
    assert.equal(storage.calls.preflights, 2);
    assert.equal(storage.calls.consumes, 2);
    assert.equal(pool.snapshot().activeLeases, 0);
  } finally {
    await runtime.close();
    await pool.close();
  }
  assert.equal(storage.calls.closes, 1);
});

test("root runtime searches the writer context as soon as it becomes available", async (t) => {
  const storage = readFixture();
  const pool = new EmbeddingModelPool({
    createModel: () => new FakeEmbeddingModel(),
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: modelLoadRequest("deterministic"),
    openSession: () => {
      assert.fail("the ready writer must serve both preflight and consumption");
    },
  });
  let releaseWriterContext;
  t.after(async () => {
    await releaseWriterContext?.();
    runtime.setWriterPending(false);
    try {
      await runtime.close();
    } finally {
      await pool.close();
    }
  });

  runtime.setWriterPending(true);
  let searchSettled = false;
  const search = runtime.search({ query: "eventual" }).then((result) => {
    searchSettled = true;
    return result;
  });
  search.catch(() => {});
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(searchSettled, false);

  releaseWriterContext = runtime.setWriterContext(
    async (options, prepared) => {
      assert.equal(options.root, "/tmp/repo");
      assert.equal(options.autoUpdate, false);
      assert.equal(pool.snapshot().activeLeases, 0);
      assert.equal(prepared.plan.options.query, options.query);
      return storage.contextPrepared(prepared);
    },
    pool.keyFor(modelLoadRequest("deterministic")),
    storage.preflight,
  );

  const result = await bounded(search, "ready writer did not serve search");
  assert.equal(searchSettled, true);
  assert.equal(result.query, "eventual");
  assert.equal(storage.calls.preflights, 1);
  assert.equal(storage.calls.consumes, 1);
  assert.equal(storage.calls.opens, 0);

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

test("root runtime does not reuse a writer context with a different model runtime", async (t) => {
  const storage = readFixture();
  const modelPrepared = deferred();
  const modelLoads = [];
  const pool = new EmbeddingModelPool({
    keyForRequest: (request) => request.runtime.apiKey,
    createModel: (request) => {
      modelLoads.push(request.runtime.apiKey);
      const model = new FakeEmbeddingModel();
      const doEmbed = model.doEmbed.bind(model);
      model.doEmbed = async (contents, options) => {
        const result = await doEmbed(contents, options);
        modelPrepared.resolve();
        return result;
      };
      return model;
    },
  });
  const writerRequest = {
    ...modelLoadRequest("deterministic"),
    runtime: { apiKey: "writer-key" },
  };
  const searchRequest = {
    ...modelLoadRequest("deterministic"),
    runtime: { apiKey: "search-key" },
  };
  let writerSearches = 0;
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: writerRequest,
    openSession: storage.openSession,
  });
  let releaseWriterContext;
  t.after(async () => {
    await releaseWriterContext?.();
    runtime.setWriterPending(false);
    try {
      await runtime.close();
    } finally {
      await pool.close();
    }
  });

  runtime.setWriterPending(true);
  releaseWriterContext = runtime.setWriterContext(
    async (_options, prepared) => {
      writerSearches += 1;
      return storage.contextPrepared(prepared);
    },
    pool.keyFor(writerRequest),
    storage.preflight,
  );
  runtime.updateModelLoadRequest(searchRequest);
  let searchSettled = false;
  const search = runtime
    .search({ query: "connection pool" }, searchRequest)
    .then((result) => {
      searchSettled = true;
      return result;
    });
  search.catch(() => {});
  await bounded(modelPrepared.promise, "query embedding did not complete");
  await new Promise((resolve) => setImmediate(resolve));
  const writerSearchesBeforeRelease = writerSearches;
  assert.equal(searchSettled, false);
  assert.equal(
    storage.calls.preflights,
    1,
    "model-free writer preflight is safe",
  );
  assert.equal(storage.calls.opens, 0);
  assert.equal(pool.snapshot().activeLeases, 0);

  await releaseWriterContext();
  runtime.setWriterPending(false);
  const result = await bounded(
    search,
    "released writer did not unblock search",
  );
  await runtime.close();
  await pool.close();

  assert.equal(writerSearchesBeforeRelease, 0);
  assert.deepEqual(modelLoads, ["search-key"]);
  assert.equal(result.query, "connection pool");
  assert.equal(storage.calls.opens, 1);
  assert.equal(storage.calls.consumes, 1);
});

test("root runtime releases its daemon lease when read cache close fails", async () => {
  let releases = 0;
  const storage = readFixture();
  storage.close = async () => {
    throw new Error("session close failed");
  };
  const pool = new EmbeddingModelPool({
    createModel: () => new FakeEmbeddingModel(),
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: modelLoadRequest("deterministic"),
    rootLease: {
      root: "/tmp/repo",
      release: async () => {
        releases += 1;
      },
    },
    readSessionIdleTtlMs: 60_000,
    openSession: storage.openSession,
  });
  await runtime.search({ query: "query" });
  await assert.rejects(runtime.close(), /session close failed/);
  await assert.rejects(runtime.close(), /session close failed/);
  assert.equal(releases, 1);
  assert.equal(storage.calls.closes, 1);
  assert.equal(pool.snapshot().activeLeases, 0);
  await pool.close();
});

test("root runtime executes prepared FTS without a model request or lease", async () => {
  const storage = readFixture();
  const pool = new EmbeddingModelPool({
    createModel: () => assert.fail("FTS must never acquire an embedding model"),
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    openSession: storage.openSession,
  });
  try {
    const result = await runtime.search({
      routes: [{ mode: "fts", query: "connectionPool" }],
    });
    assert.equal(result.query, "connectionPool");
    assert.equal(storage.calls.preflights, 1);
    assert.equal(storage.calls.consumes, 1);
    assert.ok(storage.calls.fts > 0);
    assert.equal(storage.calls.vector, 0);
    assert.deepEqual(pool.snapshot(), { loaded: 0, activeLeases: 0 });
  } finally {
    await runtime.close();
    await pool.close();
  }
});

for (const phase of ["load", "embed"]) {
  test(`root runtime close drains pending model ${phase} without reopening storage or leaking a lease`, async () => {
    const storage = readFixture();
    const entered = deferred();
    const release = deferred();
    let modelLoads = 0;
    let modelDisposals = 0;
    let rootReleases = 0;
    const model = new FakeEmbeddingModel();
    const doEmbed = model.doEmbed.bind(model);
    model.doEmbed = async (contents, options) => {
      if (phase === "embed") {
        entered.resolve();
        await release.promise;
      }
      return doEmbed(contents, options);
    };
    model.dispose = async () => {
      modelDisposals += 1;
    };
    const pool = new EmbeddingModelPool({
      idleTtlMs: 0,
      createModel: async () => {
        modelLoads += 1;
        if (phase === "load") {
          entered.resolve();
          await release.promise;
        }
        return model;
      },
    });
    const runtime = new RootRuntime({
      canonicalRoot: "/tmp/repo",
      modelPool: pool,
      modelLoadRequest: modelLoadRequest("deterministic"),
      openSession: storage.openSession,
      readSessionIdleTtlMs: 60_000,
      rootLease: {
        root: "/tmp/repo",
        release: async () => {
          rootReleases += 1;
        },
      },
    });
    const search = runtime.search({ query: "connection pool" });
    // Observe rejection from the start, before triggering close, so a prompt
    // rejection cannot become an unrelated unhandled-rejection test failure.
    const outcome = search.then(
      (value) => ({ value }),
      (error) => ({ error }),
    );
    try {
      await bounded(entered.promise, `model ${phase} did not start`);
      assert.equal(storage.calls.opens, 1);
      assert.equal(storage.calls.preflights, 1);
      assert.equal(runtime.snapshot().activeReaders, 0);
      assert.equal(pool.snapshot().activeLeases, phase === "embed" ? 1 : 0);
      let closeSettled = false;
      const closing = runtime.close().then(() => {
        closeSettled = true;
      });
      await new Promise((resolve) => setImmediate(resolve));
      assert.equal(closeSettled, false, "close must drain active preparation");
      assert.equal(storage.calls.closes, 0);
      assert.equal(rootReleases, 0);
      release.resolve();
      const result = await bounded(outcome, "closed search did not settle");
      assert.match(result.error?.message ?? "", /Root runtime is closed/);
      await bounded(closing, "runtime close did not drain preparation");
      await assert.rejects(runtime.search({ query: "after close" }), /closed/);
      assert.equal(storage.calls.opens, 1, "closed preparation cannot reopen");
      assert.equal(storage.calls.consumes, 0);
      assert.equal(storage.calls.closes, 1);
      assert.equal(rootReleases, 1);
      assert.equal(modelLoads, 1);
      assert.equal(modelDisposals, 1);
      assert.deepEqual(pool.snapshot(), { loaded: 0, activeLeases: 0 });
    } finally {
      release.resolve();
      await bounded(Promise.allSettled([outcome, runtime.close()]), "cleanup");
      await pool.close();
    }
  });
}

test("root close remains single-flight when a model abort listener reenters close", async () => {
  const storage = readFixture();
  const entered = deferred();
  const release = deferred();
  let releases = 0;
  let reentered;
  const model = new FakeEmbeddingModel();
  const embed = model.doEmbed.bind(model);
  model.doEmbed = async (contents, options) => {
    options.signal.addEventListener(
      "abort",
      () => {
        reentered = runtime.close();
        release.resolve();
      },
      { once: true },
    );
    entered.resolve();
    await release.promise;
    return embed(contents, options);
  };
  const pool = new EmbeddingModelPool({ createModel: () => model });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: modelLoadRequest("deterministic"),
    openSession: storage.openSession,
    rootLease: {
      root: "/tmp/repo",
      release: async () => {
        releases += 1;
      },
    },
  });
  const search = runtime.search({ query: "connection pool" });
  const outcome = search.catch((error) => error);
  try {
    await bounded(entered.promise, "embedding did not start");
    await bounded(runtime.close(), "close did not cancel preparation");
    await reentered;
    assert.match((await outcome).message, /Root runtime is closed/);
    assert.equal(releases, 1);
    assert.equal(storage.calls.closes, 1);
    assert.equal(pool.snapshot().activeLeases, 0);
  } finally {
    release.resolve();
    await Promise.allSettled([outcome, reentered, runtime.close()]);
    await pool.close();
  }
});

for (const phase of ["preflight", "consume"]) {
  for (const cancellation of ["shutdown", "caller"]) {
    test(`root close drains actual writer reads during ${phase} after ${cancellation} cancellation`, async () => {
      const storage = readFixture();
      const entered = deferred();
      const released = deferred();
      const controller = new AbortController();
      const reason = new Error("caller cancelled the writer read");
      let actualFinished = false;
      let closeSettled = false;
      let leaseReleases = 0;
      const pool = new EmbeddingModelPool({
        createModel: () => assert.fail("writer FTS must not load a model"),
      });
      const runtime = new RootRuntime({
        canonicalRoot: "/tmp/repo",
        modelPool: pool,
        openSession: () =>
          assert.fail("the writer must serve both read phases"),
        rootLease: {
          root: "/tmp/repo",
          release: async () => {
            leaseReleases += 1;
          },
        },
      });
      const holdRead = async (operation) => {
        entered.resolve();
        try {
          await released.promise;
          return await operation();
        } finally {
          actualFinished = true;
        }
      };
      // Register directly: RootRuntime.close must own its read drain without
      // depending on an outer backend scheduler or withWrite operation.
      const unregister = runtime.setWriterContext(
        (_options, prepared) =>
          phase === "consume"
            ? holdRead(() => storage.contextPrepared(prepared))
            : storage.contextPrepared(prepared),
        "writer-fts",
        (plan) =>
          phase === "preflight"
            ? holdRead(() => storage.preflight(plan))
            : storage.preflight(plan),
      );
      let outcome;
      let closing;
      try {
        outcome = runtime
          .search(
            { routes: [{ mode: "fts", query: "connectionPool" }] },
            undefined,
            { signal: controller.signal },
          )
          .then(
            () => ({ fulfilled: true }),
            (error) => ({ error }),
          );
        await bounded(entered.promise, "writer read did not enter its gate");
        if (cancellation === "caller") {
          controller.abort(reason);
          assert.equal(
            (await bounded(outcome, "caller cancellation did not settle"))
              .error,
            reason,
          );
        }
        closing = runtime.close().then(() => {
          closeSettled = true;
        });
        void closing.catch(() => {});
        const result = await bounded(outcome, "shutdown did not cancel search");
        if (cancellation === "shutdown") {
          assert.match(result.error?.message ?? "", /Root runtime is closed/);
        }
        await new Promise((resolve) => setImmediate(resolve));
        assert.deepEqual(
          { actualFinished, closeSettled, leaseReleases },
          { actualFinished: false, closeSettled: false, leaseReleases: 0 },
          "close must retain the daemon lease until the actual writer read settles",
        );

        released.resolve();
        await bounded(closing, "close did not drain the released writer read");
        await unregister();
        assert.equal(actualFinished, true);
        assert.equal(leaseReleases, 1);
        await runtime.close();
        assert.equal(leaseReleases, 1, "repeated close must not release twice");
        assert.equal(storage.calls.preflights, 1);
        assert.equal(storage.calls.consumes, phase === "consume" ? 1 : 0);
        assert.equal(storage.calls.opens, 0);
        assert.deepEqual(pool.snapshot(), { loaded: 0, activeLeases: 0 });
      } finally {
        released.resolve();
        try {
          await bounded(
            Promise.allSettled([
              outcome,
              closing,
              unregister(),
              runtime.close(),
            ]),
            "writer read cleanup did not drain",
          );
        } finally {
          await pool.close();
        }
      }
    });
  }
}

for (const changesAgain of [false, true]) {
  test(`root runtime ${changesAgain ? "stops after one retry if an empty filter changes twice" : "reprepares once when an empty filter gains files"}`, async () => {
    const storage = readFixture();
    storage.files.length = 0;
    let modelLoads = 0;
    const queries = [];
    const pool = new EmbeddingModelPool({
      createModel: () => {
        modelLoads += 1;
        const model = new FakeEmbeddingModel();
        const doEmbed = model.doEmbed.bind(model);
        model.doEmbed = async (contents, options) => {
          queries.push(...contents.map((content) => content.text));
          return doEmbed(contents, options);
        };
        return model;
      },
    });
    const runtime = new RootRuntime({
      canonicalRoot: "/tmp/repo",
      modelPool: pool,
      modelLoadRequest: modelLoadRequest("deterministic"),
      openSession: storage.openSession,
    });
    storage.beforePreflight = () => {
      if (changesAgain) storage.files.length = 0;
    };
    storage.beforeConsume = () => {
      if (storage.files.length === 0) storage.files.push(indexedFile());
    };
    try {
      const search = runtime.search({
        query: "connection pool",
        includePaths: ["src/**"],
      });
      if (changesAgain) {
        await assert.rejects(search, {
          code: "ZVEC_GREP.ENGINE.SEARCH.PREPARED_VECTORS_REQUIRED",
        });
        assert.equal(modelLoads, 0);
        assert.deepEqual(queries, []);
        assert.equal(storage.calls.fts, 0);
        assert.equal(storage.calls.vector, 0);
      } else {
        const result = await search;
        assert.equal(result.query, "connection pool");
        assert.equal(modelLoads, 1);
        assert.deepEqual(queries, ["connection pool"]);
        assert.ok(storage.calls.fts > 0);
        assert.ok(storage.calls.vector > 0);
      }
      assert.equal(storage.calls.preflights, 2);
      assert.equal(storage.calls.consumes, 2);
      assert.equal(storage.calls.opens, 1);
      assert.equal(pool.snapshot().activeLeases, 0);
    } finally {
      await runtime.close();
      await pool.close();
    }
  });
}

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

test("root runtime distinguishes routine probes from known stale evidence", async () => {
  const pool = new EmbeddingModelPool({
    createModel: () => ({ dispose: async () => {} }),
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: modelLoadRequest("model-a"),
  });

  await runtime.probeInitialFreshness(async () => true);
  runtime.requireFullReconciliation(true);
  assert.equal(runtime.needsReconciliation(), true);
  assert.equal(runtime.hasKnownChanges(), false);

  runtime.markDirty();
  assert.equal(runtime.hasKnownChanges(), true);
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

test("local preparation budget checks the actual preflight provider after a rebuild", async (t) => {
  const storage = readFixture();
  storage.workspaceIndex.embedding.provider = "qwen";
  const entered = deferred();
  const released = deferred();
  const pool = new EmbeddingModelPool({
    createModel: async () => {
      entered.resolve();
      await released.promise;
      const model = new FakeEmbeddingModel();
      model.info = { ...model.info, provider: "local" };
      return model;
    },
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: { model: { provider: "local", name: "deterministic" } },
    openSession: storage.openSession,
  });
  t.after(async () => {
    released.resolve();
    await runtime.close();
    await pool.close();
  });
  let settled = false;
  const search = runtime.search({ query: "connection pool" }, undefined, {
    semanticBudgetMs: 30,
  });
  void search.then(
    () => {
      settled = true;
    },
    () => {
      settled = true;
    },
  );
  await bounded(entered.promise, "model load never started");
  await new Promise((resolve) => setTimeout(resolve, 90));
  assert.equal(settled, false, "nonlocal preflight must not opt into fallback");
  assert.equal(storage.calls.fts, 0);
  released.resolve();
  await assert.rejects(search, {
    code: "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.EMBEDDING_PROVIDER_MISMATCH",
  });
});

test("local budget fallback rejects a provider change while the model was loading", async (t) => {
  const storage = readFixture();
  storage.workspaceIndex.embedding.provider = "local";
  const entered = deferred();
  const released = deferred();
  const pool = new EmbeddingModelPool({
    createModel: async () => {
      entered.resolve();
      await released.promise;
      return new FakeEmbeddingModel();
    },
  });
  const runtime = new RootRuntime({
    canonicalRoot: "/tmp/repo",
    modelPool: pool,
    modelLoadRequest: { model: { provider: "local", name: "deterministic" } },
    openSession: storage.openSession,
  });
  t.after(async () => {
    released.resolve();
    await runtime.close();
    await pool.close();
  });
  const search = runtime.search({ query: "connection pool" }, undefined, {
    semanticBudgetMs: 60,
  });
  search.catch(() => {});
  await bounded(entered.promise, "model load never started");
  await runtime.withWrite(async () => {
    storage.workspaceIndex.embedding = {
      ...storage.workspaceIndex.embedding,
      provider: "qwen",
    };
  });
  await assert.rejects(
    bounded(search, "changed provider did not terminate the fallback"),
    { code: "ZVEC_GREP.ENGINE.SEARCH.LOCAL_FALLBACK_PROVIDER_CHANGED" },
  );
  assert.equal(storage.calls.preflights, 2);
  assert.equal(storage.calls.consumes, 0);
  assert.equal(storage.calls.fts, 0);
  assert.equal(storage.calls.vector, 0);
});

function modelLoadRequest(model) {
  return {
    model: { provider: "test", name: model },
  };
}

function indexedFile() {
  return {
    id: "pool-file",
    absolutePath: "/tmp/repo/src/pool.ts",
    relativePath: "src/pool.ts",
    rootPath: "/tmp/repo",
    kind: "code",
    format: "typescript",
    sizeBytes: 64,
    lastModifiedTime: 1,
  };
}

/** Storage is a lifecycle mock; planning, filtering and prepared consumption
 * use the real engine, including vector presence/shape and query validation. */
function readFixture() {
  const files = [indexedFile()];
  const calls = {
    opens: 0,
    closes: 0,
    preflights: 0,
    consumes: 0,
    fts: 0,
    vector: 0,
  };
  const workspaceIndex = {
    id: "runtime-fixture",
    name: "runtime-fixture",
    path: "/tmp/repo/.zvec-grep",
    rootPaths: [{ absolutePath: "/tmp/repo", recursive: true }],
    indexVersion: CURRENT_INDEX_VERSION,
    embedding: {
      provider: "test",
      model: "deterministic",
      dimension: 16,
      metric: "cosine",
    },
    createdTime: 1,
    updatedTime: 1,
  };
  const context = {
    workspaceIndex,
    storage: {
      listFiles: () => files,
      searchFts: () => {
        calls.fts += 1;
        return [];
      },
      searchVector: () => {
        calls.vector += 1;
        return [];
      },
    },
  };
  const fixture = {
    workspaceIndex,
    files,
    calls,
    beforePreflight: undefined,
    beforeConsume: undefined,
    close: undefined,
    async preflight(plan) {
      calls.preflights += 1;
      await fixture.beforePreflight?.(plan);
      assert.equal(plan.options.root, "/tmp/repo");
      assert.equal(plan.options.autoUpdate, false);
      const searches = await Promise.all(
        plan.searches.map((search) => preflightSearchPlan(search, context)),
      );
      return {
        plan,
        searches,
        requiresEmbedding: searches.some(
          (search, index) =>
            search.hasSearchableFiles &&
            plan.searches[index].routes.some(
              (route) => route.mode === "vector",
            ),
        ),
        workspaceIndex,
      };
    },
    async contextPrepared(prepared) {
      calls.consumes += 1;
      await fixture.beforeConsume?.(prepared);
      for (const [index, search] of prepared.searches.entries()) {
        await searchWorkspaceIndex(
          prepared.plan.searches[index],
          context,
          search,
        );
      }
      return {
        ...emptyContextResult(),
        query: prepared.plan.request.displayQuery,
      };
    },
    async openSession(...args) {
      assert.deepEqual(
        args,
        [],
        "storage sessions must not borrow model leases",
      );
      calls.opens += 1;
      let closed = false;
      return {
        root: "/tmp/repo",
        context: () => assert.fail("runtime must use the prepared-search path"),
        preflight: (plan) => {
          assert.equal(closed, false);
          return fixture.preflight(plan);
        },
        contextPrepared: (prepared) => {
          assert.equal(closed, false);
          return fixture.contextPrepared(prepared);
        },
        close: async () => {
          assert.equal(closed, false);
          closed = true;
          calls.closes += 1;
          await fixture.close?.();
        },
      };
    },
  };
  return fixture;
}

for (const owner of ["reader", "writer"]) {
  test(`a canceled ${owner} records source drift before its actual storage ownership drains`, async () => {
    const storage = readFixture();
    const entered = deferred();
    const released = deferred();
    const controller = new AbortController();
    const reason = new Error("cancel freshness observer");
    const evidence = {
      indexed: {
        workspaceIndexId: "runtime-fixture",
        fileId: "pool-file",
        absolutePath: "/tmp/repo/pool.ts",
        rootPath: "/tmp/repo",
        indexedTime: 1,
        contentHash: "old-hash",
        sizeBytes: 10,
      },
      reason: "hash_mismatch",
      observedHash: "new-hash",
      observedSizeBytes: 10,
    };
    const pool = new EmbeddingModelPool({
      createModel: () => assert.fail("source observation FTS needs no model"),
    });
    const runtime = new RootRuntime({
      canonicalRoot: "/tmp/repo",
      modelPool: pool,
      openSession: storage.openSession,
    });
    const consume = storage.contextPrepared;
    storage.contextPrepared = async (prepared) => {
      entered.resolve();
      await released.promise;
      const result = await consume(prepared);
      return {
        ...result,
        diagnostics: { index: { sourceInvalidations: [evidence] } },
      };
    };
    const unregister =
      owner === "writer"
        ? runtime.setWriterContext(
            (_options, prepared) => storage.contextPrepared(prepared),
            "writer-fts",
            storage.preflight,
          )
        : undefined;
    let draining;
    let outcome;
    try {
      outcome = runtime
        .search({ routes: [{ mode: "fts", query: "pool" }] }, undefined, {
          signal: controller.signal,
        })
        .catch((error) => error);
      await bounded(entered.promise, "source observer never entered storage");
      controller.abort(reason);
      assert.equal(await bounded(outcome, "observer did not cancel"), reason);
      assert.equal(runtime.sourceInvalidations.hasPending(), false);
      let drained = false;
      const acknowledge = () => {
        const pending = runtime.sourceInvalidations.pending();
        assert.equal(pending.length, 1, "actual read must record before drain");
        runtime.sourceInvalidations.acknowledge(pending, {
          workspaceIndexId: "runtime-fixture",
          paths: [
            {
              absolutePath: evidence.indexed.absolutePath,
              status: "fresh",
              indexed: { ...evidence.indexed, contentHash: "new-hash" },
            },
          ],
        });
        drained = true;
      };
      draining =
        owner === "writer"
          ? unregister().then(acknowledge)
          : runtime.withWrite(async () => acknowledge());
      void draining.catch(() => {});
      await new Promise((resolve) => setImmediate(resolve));
      assert.equal(drained, false);
      released.resolve();
      await bounded(draining, "actual source observer did not drain");
      assert.equal(drained, true);
      assert.equal(runtime.sourceInvalidations.hasPending(), false);
      await new Promise((resolve) => setImmediate(resolve));
      assert.equal(
        runtime.sourceInvalidations.hasPending(),
        false,
        "a detached search must not resurrect evidence after acknowledgment",
      );
    } finally {
      released.resolve();
      await Promise.allSettled([outcome, draining, unregister?.()]);
      await runtime.close();
      await pool.close();
    }
  });
}

function deferred() {
  let resolve;
  const promise = new Promise((complete) => {
    resolve = complete;
  });
  return { promise, resolve };
}

async function bounded(promise, label) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_resolve, reject) => {
        timer = setTimeout(() => reject(new Error(label)), 3_000);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
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
