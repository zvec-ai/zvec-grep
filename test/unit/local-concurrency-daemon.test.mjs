import assert from "node:assert/strict";
import test from "node:test";
import { DaemonBackend } from "../../dist/daemon/backend.js";
import { EmbeddingModelPool } from "../../dist/daemon/model-pool.js";

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
