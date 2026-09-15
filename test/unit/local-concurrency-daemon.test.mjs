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

test("daemon model requests retain explicit concurrency outside persisted runtime", async (t) => {
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
  assert.equal(search.embeddingConcurrency, 3);
  assert.equal(search.runtime.embeddingConcurrency, undefined);
  assert.equal(
    backend.searchModelLoadRequest(info, {}).embeddingConcurrency,
    undefined,
  );

  const active = backend.overrideActiveModelLoadRequest(index, {
    embeddingConcurrency: 4,
  });
  assert.equal(active.embeddingConcurrency, 4);
  assert.equal(active.runtime.embeddingConcurrency, undefined);
  assert.equal(
    backend.overrideActiveModelLoadRequest(index, {}).embeddingConcurrency,
    2,
  );
  assert.equal(index.embeddingConcurrency, 2);
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

test("model pool partitions local runtime limits and honors request precedence", async (t) => {
  setEnvironment(t, {
    ZVEC_GREP_LOCAL_EMBEDDING_CONCURRENCY: "1",
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

  const inherited = await acquire(localRequest());
  assert.equal(inherited.model.info.defaultConcurrency, 2);
  const explicit = await acquire(localRequest(3));
  assert.equal(explicit.model.info.defaultConcurrency, 3);
  assert.notEqual(explicit.key, inherited.key);
  assert.notEqual(explicit.model, inherited.model);
  assert.equal((await acquire(localRequest(3))).model, explicit.model);
  assert.equal((await acquire(localRequest(2))).model, inherited.model);
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
