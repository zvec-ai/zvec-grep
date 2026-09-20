import assert from "node:assert/strict";
import test from "node:test";
import { DaemonBackend } from "../../dist/daemon/backend.js";
import { EmbeddingModelPool } from "../../dist/daemon/model-pool.js";
import { RootRuntime } from "../../dist/daemon/root-runtime.js";

function setEnvironment(t, values) {
  for (const [name, value] of Object.entries(values)) {
    const previous = process.env[name];
    if (value === undefined) delete process.env[name];
    else process.env[name] = value;
    t.after(() => {
      if (previous === undefined) delete process.env[name];
      else process.env[name] = previous;
    });
  }
}

function localRequest(embeddingConcurrency) {
  return {
    model: { provider: "local", name: "bge-small-en-v1.5" },
    runtime: { device: "cpu" },
    ...(embeddingConcurrency !== undefined ? { embeddingConcurrency } : {}),
  };
}

function deferred() {
  let resolve;
  const promise = new Promise((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

function daemonLifecycle(t, hooks = {}) {
  const events = [];
  const resident = new Set();
  const name = hooks.name ?? "bge-small-en-v1.5";
  const info = {
    root: "/tmp/zvec-grep-concurrency-test",
    indexed: true,
    workspaceIndex: {
      embedding: {
        provider: "local",
        model: name,
        dimension: 384,
        metric: "cosine",
      },
    },
  };
  const backend = new DaemonBackend({
    version: "test",
    modelPoolOptions: {
      maxLoadedModels: 1,
      idleTtlMs: 60_000,
      createModel: (request) => {
        const concurrency = request.embeddingConcurrency ?? 1;
        events.push(`create:${concurrency}`);
        hooks.createModel?.(concurrency);
        const model = {
          async embedQuery() {
            load();
            await hooks.query?.();
            return [1];
          },
          async embedDocuments() {
            load();
            return [[1]];
          },
          async dispose() {
            events.push(`dispose:start:${concurrency}`);
            await hooks.dispose?.(concurrency);
            resident.delete(model);
            events.push(`dispose:end:${concurrency}`);
          },
        };
        function load() {
          if (resident.has(model)) return;
          assert.equal(resident.size, 0, "old native model is still resident");
          resident.add(model);
          events.push(`load:${concurrency}`);
        }
        return model;
      },
    },
    createService: async (options) => {
      await hooks.createService?.();
      return {
        index: async () => {
          hooks.index?.();
          await options.embeddingModel.embedDocuments(["document"]);
          return {};
        },
        close: async () => {
          events.push("service.close");
          await hooks.serviceClose?.();
        },
      };
    },
  });
  t.mock.method(backend, "inspectRoot", async () => info);
  t.mock.method(backend, "readWorkspaceEmbeddingRuntime", () => ({
    device: "cpu",
  }));
  const queryRequest = backend.searchModelLoadRequest(info, {});
  const runtime = new RootRuntime({
    canonicalRoot: info.root,
    modelPool: backend.modelPool,
    modelLoadRequest: queryRequest,
    readSessionIdleTtlMs: 60_000,
    openSession: (lease) => ({
      root: info.root,
      context: async () => {
        await lease.model.embedQuery("query");
        return { items: [] };
      },
      close: async () => {
        events.push("session.close");
      },
    }),
  });
  t.after(async () => {
    await runtime.close();
    await backend.close();
  });
  return {
    backend,
    runtime,
    events,
    resident,
    query: () => runtime.search({ query: "query" }, queryRequest),
    index: (input = {}) =>
      backend.runIndexOperation(
        runtime,
        { changedPaths: ["fixture.ts"], ...input },
        () => {},
      ),
  };
}

for (const name of ["bge-small-en-v1.5", "qwen3-embedding-0.6b"]) {
  for (const source of ["explicit", "environment"]) {
    test(`daemon releases cached query model before ${source} indexing (${name})`, async (t) => {
      setEnvironment(t, {
        ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY:
          source === "environment" ? "3" : undefined,
        ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM: undefined,
      });
      const disposing = deferred();
      const releaseDisposal = deferred();
      t.after(() => releaseDisposal.resolve());
      let blockDisposal = true;
      const fixture = daemonLifecycle(t, {
        name,
        dispose: async () => {
          if (!blockDisposal) return;
          blockDisposal = false;
          disposing.resolve();
          await releaseDisposal.promise;
        },
      });
      await fixture.query();
      assert.equal(fixture.runtime.snapshot().readSessionOpen, true);
      assert.equal(fixture.backend.modelPool.snapshot().activeLeases, 1);

      const indexInput =
        source === "explicit" ? { embeddingConcurrency: 3 } : {};
      const indexing = fixture.index(indexInput);
      await Promise.race([
        disposing.promise,
        indexing.then(() => assert.fail("indexing bypassed model disposal")),
      ]);
      assert.equal(fixture.runtime.snapshot().readSessionOpen, false);
      assert.equal(fixture.runtime.snapshot().writerPending, true);
      assert.equal(fixture.events.includes("load:3"), false);
      assert.equal(fixture.resident.size, 1);
      releaseDisposal.resolve();
      await indexing;
      assert.equal(fixture.backend.modelPool.snapshot().activeLeases, 0);

      await fixture.query();
      await fixture.index(indexInput);
      await fixture.query();
      assert.equal(fixture.resident.size, 1);
      assert.equal(fixture.backend.modelPool.snapshot().loaded, 1);
      assert.deepEqual(
        fixture.events.filter((event) => event.startsWith("load:")),
        ["load:1", "load:3", "load:1", "load:3", "load:1"],
      );
    });
  }
}

test("daemon waits for an active query before loading the indexing model", async (t) => {
  setEnvironment(t, { ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY: "3" });
  const queryStarted = deferred();
  const releaseQuery = deferred();
  t.after(() => releaseQuery.resolve());
  const fixture = daemonLifecycle(t, {
    query: async () => {
      queryStarted.resolve();
      await releaseQuery.promise;
    },
  });
  const querying = fixture.query();
  await queryStarted.promise;
  const indexing = fixture.index();
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(fixture.runtime.snapshot().writerPending, true);
  assert.equal(fixture.events.includes("load:3"), false);
  releaseQuery.resolve();
  await querying;
  await indexing;
  assert.equal(fixture.backend.modelPool.snapshot().activeLeases, 0);
  assert.equal(fixture.resident.size, 1);
});

test("daemon keeps cached queries available during index service preparation", async (t) => {
  setEnvironment(t, { ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY: "3" });
  const preparing = deferred();
  const releasePreparation = deferred();
  t.after(() => releasePreparation.resolve());
  const fixture = daemonLifecycle(t, {
    createService: async () => {
      preparing.resolve();
      await releasePreparation.promise;
    },
  });
  await fixture.query();
  const indexing = fixture.index();
  await preparing.promise;
  assert.equal(fixture.runtime.snapshot().writerPending, false);
  await fixture.query();
  assert.equal(fixture.events.includes("load:3"), false);
  releasePreparation.resolve();
  await indexing;
});

test("daemon releases the index model lease before a queued query can load", async (t) => {
  setEnvironment(t, { ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY: "3" });
  const closing = deferred();
  const releaseClose = deferred();
  t.after(() => releaseClose.resolve());
  const fixture = daemonLifecycle(t, {
    serviceClose: async () => {
      closing.resolve();
      await releaseClose.promise;
    },
  });
  await fixture.query();
  const indexing = fixture.index();
  await closing.promise;
  const querying = fixture.query();
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(fixture.runtime.snapshot().writerPending, true);
  assert.deepEqual(
    fixture.events.filter((event) => event.startsWith("load:")),
    ["load:1", "load:3"],
  );
  releaseClose.resolve();
  await indexing;
  await querying;
  assert.equal(fixture.backend.modelPool.snapshot().loaded, 1);
  assert.equal(fixture.backend.modelPool.snapshot().activeLeases, 1);
  assert.equal(fixture.resident.size, 1);
});

for (const failingStage of [
  "createModel",
  "createService",
  "index",
  "serviceClose",
]) {
  test(`daemon releases its writer and model lease after ${failingStage} fails`, async (t) => {
    setEnvironment(t, { ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY: "3" });
    let fail = false;
    const fixture = daemonLifecycle(t, {
      [failingStage]: () => {
        if (fail) throw new Error(`${failingStage} failed`);
      },
    });
    await fixture.query();
    fail = true;
    await assert.rejects(fixture.index(), new RegExp(`${failingStage} failed`));
    assert.equal(fixture.runtime.snapshot().writerPending, false);
    assert.equal(
      fixture.backend.modelPool.snapshot().activeLeases,
      ["index", "serviceClose"].includes(failingStage) ? 0 : 1,
    );

    fail = false;
    await fixture.query();
    await fixture.index();
    await fixture.query();
    assert.equal(fixture.backend.modelPool.snapshot().loaded, 1);
    assert.equal(fixture.resident.size, 1);
  });
}

test("daemon index limits never enter query models or persisted runtime", async (t) => {
  setEnvironment(t, {
    ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY: "7",
    ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM: "8",
  });
  const backend = new DaemonBackend({ version: "test" });
  t.after(() => backend.close());
  t.mock.method(backend, "readWorkspaceEmbeddingRuntime", () => ({
    device: "cuda",
  }));
  const info = {
    root: "/tmp/zvec-grep-concurrency-test",
    indexed: true,
    workspaceIndex: {
      embedding: {
        provider: "local",
        model: "qwen3-embedding-0.6b",
        dimension: 1024,
        metric: "cosine",
      },
    },
  };

  const index = backend.indexModelLoadRequest(info, {
    embeddingConcurrency: 2,
  });
  assert.equal(index.embeddingConcurrency, 2);
  assert.equal(index.runtime.embeddingConcurrency, undefined);
  assert.equal(index.runtime.device, "cuda");

  const search = backend.searchModelLoadRequest(info, {
    embeddingConcurrency: 3,
  });
  assert.equal(search.embeddingConcurrency, undefined);
  assert.equal(search.runtime.embeddingConcurrency, undefined);
  assert.equal(
    backend.searchModelLoadRequest(info, {}).embeddingConcurrency,
    undefined,
  );

  const active = backend.overrideActiveModelLoadRequest(index, {
    embeddingConcurrency: 4,
  });
  assert.equal(active.embeddingConcurrency, undefined);
  assert.equal(active.runtime.embeddingConcurrency, undefined);
  assert.equal(
    backend.overrideActiveModelLoadRequest(index, {}).embeddingConcurrency,
    undefined,
  );
  assert.equal(index.embeddingConcurrency, 2);
  assert.notEqual(
    backend.modelPool.keyFor(index),
    backend.modelPool.keyFor(search),
  );
  assert.equal(
    backend.modelPool.keyFor(active),
    backend.modelPool.keyFor(search),
  );
});

test("daemon resolves index defaults for local and remote models before loading or scheduling", async (t) => {
  setEnvironment(t, {
    ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY: "12",
    ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM: "8",
  });
  const backend = new DaemonBackend({ version: "test" });
  const configured = new DaemonBackend({
    version: "test",
    serviceOptions: { embeddingConcurrency: 5 },
  });
  t.after(async () => {
    await backend.close();
    await configured.close();
  });
  const info = { root: "/tmp/zvec-grep-concurrency-test", indexed: false };

  for (const embedding of [
    "local/qwen3-embedding-0.6b",
    "local/bge-small-en-v1.5",
    "local/potion-code-16m-v2",
  ]) {
    assert.equal(
      backend.indexModelLoadRequest(info, { embedding }).embeddingConcurrency,
      12,
    );
    assert.equal(
      configured.indexModelLoadRequest(info, { embedding })
        .embeddingConcurrency,
      5,
    );
    assert.equal(
      configured.indexModelLoadRequest(info, {
        embedding,
        embeddingConcurrency: 3,
      }).embeddingConcurrency,
      3,
    );
  }

  const remote = { embedding: "qwen/text-embedding-v4", apiKey: "test-key" };
  assert.equal(
    backend.indexModelLoadRequest(info, remote).embeddingConcurrency,
    12,
  );
  assert.equal(
    backend.indexModelLoadRequest(info, { ...remote, embeddingConcurrency: 3 })
      .embeddingConcurrency,
    3,
  );
});

test("fresh search reconciliation forwards the request concurrency", async (t) => {
  const backend = new DaemonBackend({ version: "test" });
  t.after(() => backend.close());
  let needsReconciliation = true;
  const runtime = {
    canonicalRoot: "/tmp/zvec-grep-concurrency-test",
    needsReconciliation: () => needsReconciliation,
    requiresFullReconciliation: () => true,
    canProbeFullReconciliation: () => false,
  };
  const submitted = [];
  t.mock.method(backend, "settleKnownChanges", async () => {});
  t.mock.method(backend.scheduler, "getByRoot", () => undefined);
  t.mock.method(
    backend,
    "submitIndex",
    async (_runtime, input, reason, wait) => {
      submitted.push({ input, reason, wait });
      needsReconciliation = false;
      return { state: "succeeded" };
    },
  );

  await backend.waitForFresh(runtime, undefined, {
    device: "cuda",
    embeddingConcurrency: 2,
  });
  assert.deepEqual(submitted, [
    {
      input: {
        root: runtime.canonicalRoot,
        apiKey: undefined,
        device: "cuda",
        embeddingConcurrency: 2,
        runtimeOverridesAreEphemeral: true,
      },
      reason: "fresh_query",
      wait: true,
    },
  ]);
});

test("daemon uses one resolved index limit for the model and batch scheduler", async (t) => {
  setEnvironment(t, { ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY: "12" });
  let createdOptions;
  let indexedOptions;
  const backend = new DaemonBackend({
    version: "test",
    createService: async (options) => {
      createdOptions = options;
      return {
        index: async (indexOptions) => {
          indexedOptions = indexOptions;
          return {};
        },
        close: async () => {},
      };
    },
  });
  t.after(() => backend.close());
  const info = {
    root: "/tmp/zvec-grep-concurrency-test",
    indexed: true,
    workspaceIndex: {
      embedding: {
        provider: "local",
        model: "bge-small-en-v1.5",
        dimension: 384,
        metric: "cosine",
      },
    },
  };
  t.mock.method(backend, "inspectRoot", async () => info);
  t.mock.method(backend, "readWorkspaceEmbeddingRuntime", () => ({}));
  let loadedRequest;
  t.mock.method(backend.modelPool, "acquire", async (request) => {
    loadedRequest = request;
    process.env.ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY = "3";
    return { model: {}, key: "test-key", release() {} };
  });
  const requests = [];
  const runtime = {
    canonicalRoot: info.root,
    updateModelLoadRequest: (request) => requests.push(request),
    withWrite: (operation) => operation(),
    reconciliationEpoch: () => 1,
  };
  await backend.runIndexOperation(
    runtime,
    { changedPaths: ["fixture.ts"] },
    () => {},
  );
  assert.equal(loadedRequest.embeddingConcurrency, 12);
  assert.equal(createdOptions.embeddingConcurrency, 12);
  assert.equal(indexedOptions.embeddingConcurrency, 12);
  assert.equal(requests.at(-1).embeddingConcurrency, undefined);
});

test("model pool isolates query defaults from index settings and model instances", async (t) => {
  setEnvironment(t, {
    ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY: "7",
    ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM: "8",
  });
  const pool = new EmbeddingModelPool({
    serviceOptions: { embeddingConcurrency: 2 },
  });
  const leases = [];
  t.after(async () => {
    for (const lease of leases) lease.release();
    await pool.close();
  });
  const acquire = async (request) => {
    const lease = await pool.acquire(request);
    leases.push(lease);
    return lease;
  };

  const query = await acquire(localRequest());
  assert.equal(query.model.info.defaultConcurrency, 1);
  const explicit = await acquire(localRequest(3));
  assert.equal(explicit.model.info.defaultConcurrency, 3);
  assert.notEqual(explicit.key, query.key);
  assert.notEqual(explicit.model, query.model);
  assert.equal((await acquire(localRequest(3))).model, explicit.model);
  assert.equal((await acquire(localRequest())).model, query.model);
  assert.equal(pool.snapshot().loaded, 2);
});

test("local concurrency does not split Potion or remote model cache keys", () => {
  const pool = new EmbeddingModelPool();
  for (const model of [
    { provider: "local", name: "potion-code-16m-v2" },
    { provider: "qwen", name: "text-embedding-v4" },
  ]) {
    assert.equal(
      pool.keyFor({ model, embeddingConcurrency: 1 }),
      pool.keyFor({ model, embeddingConcurrency: 8 }),
    );
  }
});
