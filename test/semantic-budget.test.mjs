import assert from "node:assert/strict";
import { mkdir, mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { DaemonBackend } from "../dist/daemon/backend.js";
import { createZvecGrep } from "../dist/index.js";
import { FakeEmbeddingModel } from "./helpers/fake-embedding.mjs";

const BUDGET_MS = 150;
const noopWatchManagerFactory = () => ({
  start() {},
  flushPending: async () => {},
  close: async () => {},
});

class FakeLocalModel extends FakeEmbeddingModel {
  info = {
    ...this.info,
    reference: "local/potion-code-16m-v2",
    provider: "local",
    name: "potion-code-16m-v2",
  };
}

test("the same ordinary hybrid query returns primary-group FTS after its local preparation budget", async (t) => {
  const entered = deferred();
  const released = deferred();
  const aborted = deferred();
  let queryCalls = 0;
  class Model extends FakeLocalModel {
    async doEmbed(contents, options) {
      queryCalls += 1;
      entered.resolve();
      try {
        await waitForAbortOrRelease(options.signal, released.promise);
      } finally {
        if (options.signal?.aborted) aborted.resolve();
      }
      return super.doEmbed(contents, options);
    }
  }
  const rig = await createRig(t, {
    createModel: () => new Model(),
    releases: [released],
  });
  const searching = rig.observe(rig.backend.search(ordinaryInput(rig.root)));
  await deadline(entered.promise, "ordinary query never reached its model");
  const response = await deadline(
    searching,
    "ordinary query waited indefinitely for semantics",
  );
  await deadline(
    aborted.promise,
    "budget did not propagate cancellation to the model",
  );
  assertPartial(response);
  assertContains(response, "pool-current");
  assert.equal(response.result.query, "connectionPool");
  assert.deepEqual(
    response.result.groupResults.map(({ id, query, role }) => ({
      id,
      query,
      role,
    })),
    [{ id: "Q1", query: "connectionPool", role: "primary" }],
  );
  assert.ok(
    response.result.items.every((item) =>
      item.queryGroups.some(
        (group) => group.id === "Q1" && group.role === "primary",
      ),
    ),
  );
  assert.ok(
    response.result.diagnostics.index.routes.every(
      (route) => route.mode === "fts",
    ),
  );
  assert.equal(queryCalls, 1);
  await eventually(
    () => rig.backend.modelPool.snapshot().activeLeases === 0,
    "cancelled preparation retained a model lease",
  );
});

test("empty FTS after the semantic budget reports incomplete search, not definitive no matches", async (t) => {
  const entered = deferred();
  const released = deferred();
  class Model extends FakeLocalModel {
    async doEmbed(contents, options) {
      entered.resolve();
      await waitForAbortOrRelease(options.signal, released.promise);
      return super.doEmbed(contents, options);
    }
  }
  const rig = await createRig(t, {
    createModel: () => new Model(),
    releases: [released],
  });
  const searching = rig.observe(
    rig.backend.search(
      ordinaryInput(rig.root, { queries: ["NoSuchIndexedPool_928341"] }),
    ),
  );
  await deadline(
    entered.promise,
    "empty-query fixture did not reach embedding",
  );
  const response = await deadline(
    searching,
    "empty FTS could not finish after the budget",
  );
  assertPartial(response);
  assert.deepEqual(response.result.items, []);
  assert.equal(response.result.diagnostics.emptyReason, "semantic_incomplete");
  assert.equal(response.result.groupResults[0].role, "primary");
});

test("fast default preparation preserves full-search results and exact embedding counters", async (t) => {
  let loads = 0;
  let queryCalls = 0;
  class Model extends FakeLocalModel {
    async doEmbed(contents, options) {
      if (options.purpose === "query") queryCalls += 1;
      return super.doEmbed(contents, options);
    }
  }
  const rig = await createRig(t, {
    createModel: () => {
      loads += 1;
      return new Model();
    },
  });
  const strict = await rig.backend.search(
    ordinaryInput(rig.root, { semanticPolicy: "wait" }),
  );
  const normal = await rig.backend.search(ordinaryInput(rig.root));
  assert.equal(normal.result.diagnostics.semantic, undefined);
  assert.deepEqual(resultShape(normal), resultShape(strict));
  assert.ok(
    normal.result.diagnostics.index.routes.some(
      (route) => route.mode === "vector",
    ),
  );
  assert.equal(loads, 1);
  assert.equal(queryCalls, 2);
  assert.equal(rig.backend.modelPool.snapshot().activeLeases, 0);
});

test("explicit vector, semantic wait, and freshness wait never silently become budgeted FTS", async (t) => {
  let gate;
  const seenSignals = [];
  class Model extends FakeLocalModel {
    async doEmbed(contents, options) {
      if (options.purpose === "query") {
        seenSignals.push(options.signal);
        gate.entered.resolve();
        await gate.released.promise;
      }
      return super.doEmbed(contents, options);
    }
  }
  const rig = await createRig(t, { createModel: () => new Model() });
  const cases = [
    { queries: [], routes: [{ mode: "vector", query: "connectionPool" }] },
    { semanticPolicy: "wait" },
    { freshness: "wait_for_fresh" },
  ];
  for (const input of cases) {
    gate = { entered: deferred(), released: deferred() };
    rig.releases.push(gate.released);
    let settled = false;
    const searching = rig.observe(
      rig.backend.search(ordinaryInput(rig.root, input)).finally(() => {
        settled = true;
      }),
    );
    await deadline(
      gate.entered.promise,
      "strict request did not reach embedding",
    );
    await delay(BUDGET_MS * 2);
    assert.equal(settled, false, JSON.stringify(input));
    assert.equal(seenSignals.at(-1)?.aborted ?? false, false);
    gate.released.resolve();
    const result = await deadline(
      searching,
      "strict request did not finish after model release",
    );
    assert.equal(result.result.diagnostics.semantic, undefined);
    assert.ok(
      result.result.diagnostics.index.routes.some(
        (route) => route.mode === "vector",
      ),
    );
  }
});

test("nonlocal index providers are never opted into the local semantic budget", async (t) => {
  for (const provider of ["test", "qwen"]) {
    const entered = deferred();
    const released = deferred();
    // Both providers are injected deterministic fakes. This verifies routing,
    // not a network integration; no grant or remote endpoint is contacted.
    const model = modelForProvider(provider);
    const embed = model.doEmbed.bind(model);
    model.doEmbed = async (contents, options) => {
      entered.resolve();
      await released.promise;
      return embed(contents, options);
    };
    const rig = await createRig(t, {
      provider,
      createModel: () => model,
      releases: [released],
    });
    let settled = false;
    const searching = rig.observe(
      rig.backend.search(ordinaryInput(rig.root)).finally(() => {
        settled = true;
      }),
    );
    await deadline(
      entered.promise,
      `${provider} request did not reach the injected model`,
    );
    await delay(BUDGET_MS * 2);
    assert.equal(
      settled,
      false,
      `${provider} must not silently use the local-only policy`,
    );
    released.resolve();
    const response = await deadline(
      searching,
      `${provider} did not finish after model release`,
    );
    assert.equal(response.result.diagnostics.semantic, undefined);
    assert.ok(
      response.result.diagnostics.index.routes.some(
        (route) => route.mode === "vector",
      ),
    );
  }
});

test("caller cancellation rejects rather than returning partial results or submitting a new auto-update job", async (t) => {
  const entered = deferred();
  const released = deferred();
  class Model extends FakeLocalModel {
    async doEmbed(contents, options) {
      entered.resolve();
      await waitForAbortOrRelease(options.signal, released.promise);
      return super.doEmbed(contents, options);
    }
  }
  const rig = await createRig(t, {
    createModel: () => new Model(),
    releases: [released],
  });
  await writeFile(
    rig.source,
    'export function connectionPool() { return "modified-unindexed-pool"; }\n',
  );
  const controller = new AbortController();
  const reason = new Error("caller cancelled this search");
  const canonicalRoot = await realpath(rig.root);
  assert.equal(rig.backend.scheduler.getByRoot(canonicalRoot), undefined);
  const searching = rig.observe(
    rig.backend.search(ordinaryInput(rig.root, { autoUpdate: true }), {
      signal: controller.signal,
    }),
  );
  await deadline(entered.promise, "cancelled request did not reach embedding");
  controller.abort(reason);
  await assert.rejects(
    deadline(searching, "caller abort did not terminate the response"),
    (error) => error === reason,
  );
  assert.equal(rig.backend.scheduler.getByRoot(canonicalRoot), undefined);
  assert.deepEqual(rig.backend.scheduler.snapshot(), { queued: 0, running: 0 });
  await eventually(
    () => rig.backend.modelPool.snapshot().activeLeases === 0,
    "caller cancellation leaked its model lease",
  );
});

test("a late shared model load never embeds the timed-out request and still serves an uncancelled waiter", async (t) => {
  const entered = deferred();
  const released = deferred();
  const sharedAcquire = deferred();
  let loads = 0;
  const queries = [];
  class Model extends FakeLocalModel {
    async doEmbed(contents, options) {
      queries.push(contents.map((content) => content.text));
      return super.doEmbed(contents, options);
    }
  }
  const rig = await createRig(t, {
    releases: [released],
    createModel: async () => {
      loads += 1;
      entered.resolve();
      await released.promise;
      return new Model();
    },
  });
  let acquisitions = 0;
  const acquire = rig.backend.modelPool.acquire.bind(rig.backend.modelPool);
  rig.backend.modelPool.acquire = (request) => {
    acquisitions += 1;
    if (acquisitions === 2) sharedAcquire.resolve();
    return acquire(request);
  };
  const timed = rig.observe(rig.backend.search(ordinaryInput(rig.root)));
  await deadline(entered.promise, "model construction was not entered");
  const partial = await deadline(
    timed,
    "model construction blocked the local fallback",
  );
  assertPartial(partial);
  assertContains(partial, "pool-current");
  assert.deepEqual(queries, []);
  const strict = rig.observe(
    rig.backend.search(
      ordinaryInput(rig.root, {
        queries: ["requestTimeout"],
        semanticPolicy: "wait",
      }),
    ),
  );
  await deadline(
    sharedAcquire.promise,
    "strict request never joined the pending model acquisition",
  );
  assert.equal(loads, 1, "the strict waiter must join the existing load");
  released.resolve();
  const result = await deadline(
    strict,
    "an uncancelled shared-load waiter could not finish",
  );
  assert.equal(result.result.diagnostics.semantic, undefined);
  assertContains(result, "timeout-current");
  assert.deepEqual(queries, [["requestTimeout"]]);
  await eventually(
    () => rig.backend.modelPool.snapshot().activeLeases === 0,
    "a late model acquisition leaked a lease",
  );
});

test("close drains ignored cancellation work, releases leases, and never re-enters storage for late vectors", async (t) => {
  const entered = deferred();
  const released = deferred();
  let disposed = 0;
  class Model extends FakeLocalModel {
    async doEmbed(contents, options) {
      entered.resolve();
      await released.promise;
      return super.doEmbed(contents, options);
    }
    async dispose() {
      disposed += 1;
    }
  }
  const rig = await createRig(t, {
    createModel: () => new Model(),
    releases: [released],
  });
  const searching = rig.observe(rig.backend.search(ordinaryInput(rig.root)));
  await deadline(
    entered.promise,
    "uncancellable query did not reach embedding",
  );
  const response = await deadline(
    searching,
    "ignored cancellation blocked the local response",
  );
  assertPartial(response);
  assert.equal(response.root, await realpath(rig.root));
  assert.equal(
    rig.backend.modelPool.snapshot().activeLeases,
    1,
    "the still-running model must retain its lease",
  );
  const runtime = rig.backend.runtimeManager.getByCanonicalRoot(response.root);
  assert.ok(runtime);
  let lateStorageEntries = 0;
  const withWorkspaceReader = runtime.withWorkspaceReader.bind(runtime);
  runtime.withWorkspaceReader = (...args) => {
    lateStorageEntries += 1;
    return withWorkspaceReader(...args);
  };
  let closed = false;
  const closing = rig.observe(
    rig.backend.close().finally(() => {
      closed = true;
    }),
  );
  await delay(20);
  assert.equal(
    closed,
    false,
    "close must wait for the ignored preparation to settle",
  );
  assert.equal(disposed, 0);
  released.resolve();
  await deadline(closing, "close did not drain released native work");
  assert.equal(
    lateStorageEntries,
    0,
    "cancelled late vectors must never consume storage",
  );
  assert.equal(runtime.snapshot().readSessionOpen, false);
  assert.deepEqual(rig.backend.modelPool.snapshot(), {
    loaded: 0,
    activeLeases: 0,
  });
  assert.equal(disposed, 1);
});

async function createRig(
  t,
  { provider = "local", createModel, releases = [] },
) {
  const directory = await mkdtemp(join(tmpdir(), "zvec-semantic-budget-"));
  const root = join(directory, "repo");
  const source = join(root, "pool.ts");
  const pending = [];
  let backend;
  t.after(async () => {
    for (const release of releases) release.resolve();
    await Promise.allSettled(pending);
    await backend?.close();
    await rm(directory, { recursive: true, force: true });
  });
  await mkdir(root);
  await writeFile(
    source,
    'export function connectionPool() { return "pool-current"; }\n',
  );
  await writeFile(
    join(root, "retry.ts"),
    'export function requestTimeout() { return "timeout-current"; }\n',
  );
  const service = await createZvecGrep({
    root,
    embeddingModel: modelForProvider(provider),
  });
  try {
    await service.index();
  } finally {
    await service.close();
  }
  backend = new DaemonBackend({
    version: "1.0.0",
    semanticPreparationBudgetMs: BUDGET_MS,
    watchManagerFactory: noopWatchManagerFactory,
    modelPoolOptions: { createModel },
  });
  return {
    root,
    source,
    backend,
    releases,
    observe(promise) {
      pending.push(promise);
      void promise.catch(() => {});
      return promise;
    },
  };
}

function modelForProvider(provider) {
  if (provider === "local") return new FakeLocalModel();
  const model = new FakeEmbeddingModel();
  if (provider === "qwen") {
    model.info = {
      ...model.info,
      reference: "qwen/text-embedding-v4",
      provider,
      name: "text-embedding-v4",
      endpoint: "https://example.invalid/embeddings",
    };
  }
  return model;
}

function ordinaryInput(root, overrides = {}) {
  return {
    root,
    queries: ["connectionPool"],
    routes: [],
    freshness: "eventual",
    autoUpdate: false,
    ...overrides,
  };
}

function resultShape(response) {
  const result = response.result;
  return {
    query: result.query,
    items: result.items,
    groups: result.groupResults,
    routes: result.diagnostics.index.routes,
  };
}

function assertContains(response, source) {
  assert.ok(
    response.result.items.some((item) => item.content.includes(source)),
    `expected source containing ${JSON.stringify(source)}`,
  );
}

function assertPartial(response) {
  assert.deepEqual(response.result.diagnostics.semantic, {
    status: "skipped",
    reason: "preparation_budget_exceeded",
    budgetMs: BUDGET_MS,
  });
}

function deferred() {
  return Promise.withResolvers();
}

async function waitForAbortOrRelease(signal, released) {
  signal?.throwIfAborted();
  let abort;
  try {
    await Promise.race([
      released,
      new Promise((_, reject) => {
        abort = () => reject(signal.reason);
        signal?.addEventListener("abort", abort, { once: true });
      }),
    ]);
    signal?.throwIfAborted();
  } finally {
    if (abort) signal?.removeEventListener("abort", abort);
  }
}

async function deadline(promise, message, timeoutMs = 6_000) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(message)), timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function eventually(predicate, message) {
  const until = Date.now() + 3_000;
  while (!predicate()) {
    assert.ok(Date.now() < until, message);
    await delay(10);
  }
}
