import assert from "node:assert/strict";
import test from "node:test";
import { DaemonBackend } from "../../dist/daemon/backend.js";

const root = process.cwd();
const nextTurn = () => new Promise((resolve) => setImmediate(resolve));

function ordinaryInput() {
  return {
    root,
    queries: ["permission validation"],
    routes: [],
    freshness: "eventual",
    autoUpdate: true,
    limit: 10,
  };
}

function outcome(promise) {
  return promise.then(
    (value) => ({ value }),
    (error) => ({ error }),
  );
}

function fixture(t, inspectRoot) {
  const backend = new DaemonBackend({
    version: "lifecycle-fixture",
    inspectRoot,
    modelPoolOptions: {
      createModel: () => {
        throw new Error("must not load a model");
      },
    },
    createService: async () => {
      throw new Error("must not create an indexed service");
    },
  });
  let activations = 0;
  t.mock.method(backend.runtimeManager, "activate", async () => {
    activations++;
    throw new Error("must not activate an indexed runtime");
  });
  return { backend, activations: () => activations };
}

for (const [name, extraInfo] of [
  ["missing schema", {}],
  ["disabled without schema", { indexPolicy: "disabled" }],
]) {
  test(`current-source ${name} authorization uses metadata only without a status scan`, async (t) => {
    const inspections = [];
    const rig = fixture(t, async (_root, _options, includeStatus) => {
      inspections.push(includeStatus);
      return { root, indexed: false, ...extraInfo };
    });
    t.after(() => rig.backend.close());
    const acquire = t.mock.method(
      rig.backend.modelPool,
      "acquire",
      async () => {
        throw new Error("authorization must not acquire a model");
      },
    );
    assert.equal(
      await rig.backend.planSearchAuthorization(ordinaryInput()),
      undefined,
    );
    assert.deepEqual(
      inspections,
      [false],
      "a second status read can create a workspace locks directory",
    );
    assert.equal(acquire.mock.callCount(), 0);
    assert.equal(rig.activations(), 0);
    assert.deepEqual(rig.backend.runtimeManager.snapshot(), {
      activeRuntimes: 0,
    });
    assert.deepEqual(rig.backend.scheduler.snapshot(), {
      queued: 0,
      running: 0,
    });
  });
}

test("current-source Qwen schema retains its existing search authorization path", async (t) => {
  const inspections = [];
  const rig = fixture(t, async (_root, _options, includeStatus) => {
    inspections.push(includeStatus);
    return {
      root,
      indexed: true,
      workspaceIndex: {
        embedding: { provider: "qwen", model: "text-embedding-v4" },
      },
    };
  });
  t.after(() => rig.backend.close());
  const request = { model: { provider: "qwen", name: "text-embedding-v4" } };
  const requestBuilder = t.mock.method(
    rig.backend,
    "searchModelLoadRequest",
    () => request,
  );
  const sentinel = new Error(
    "existing remote authorization model-info boundary",
  );
  const modelInfo = t.mock.method(
    rig.backend,
    "loadEmbeddingModelInfo",
    async (actual) => {
      assert.equal(actual, request);
      throw sentinel;
    },
  );
  await assert.rejects(
    rig.backend.planSearchAuthorization(ordinaryInput()),
    (error) => error === sentinel,
  );
  assert.deepEqual(inspections, [false, true]);
  assert.equal(requestBuilder.mock.callCount(), 1);
  assert.equal(modelInfo.mock.callCount(), 1);
  assert.equal(rig.activations(), 0);
});

test("current-source backend close is single-flight when a shutdown listener reenters close", async (t) => {
  const { backend } = fixture(t, async () => {
    throw new Error("unused metadata probe");
  });
  t.after(() => backend.close());
  let watcherCloses = 0;
  backend.watchers.set(root, {
    close: async () => {
      watcherCloses++;
    },
  });
  let reentrant;
  backend.sourceSearchShutdown.signal.addEventListener(
    "abort",
    () => {
      reentrant = backend.close();
    },
    { once: true },
  );
  await backend.close();
  await reentrant;
  assert.equal(
    watcherCloses,
    1,
    "synchronous abort callbacks must not start a second teardown",
  );
});

test("current-source backend registers metadata ownership before invoking a reentrant inspectRoot", async (t) => {
  const entered = Promise.withResolvers();
  const released = Promise.withResolvers();
  let disposed = 0;
  let closeSettled = false;
  let closing;
  const rig = fixture(t, async () => {
    closing = rig.backend.close().then(() => {
      closeSettled = true;
    });
    entered.resolve();
    try {
      await released.promise;
      return { root, indexed: false };
    } finally {
      disposed++;
    }
  });
  const search = outcome(rig.backend.search(ordinaryInput()));
  t.after(async () => {
    released.resolve();
    await search;
    await closing;
    await rig.backend.close();
  });
  await entered.promise;
  await nextTurn();
  await nextTurn();
  assert.equal(disposed, 0);
  assert.equal(
    closeSettled,
    false,
    "shutdown must retain metadata ownership registered before the user callback runs",
  );
  released.resolve();
  assert.equal((await search).error.code, "DAEMON_SHUTTING_DOWN");
  await closing;
  assert.equal(disposed, 1);
  assert.equal(rig.activations(), 0);
});

test("current-source backend shutdown drains an already pending metadata probe without opening a runtime", async (t) => {
  const entered = Promise.withResolvers();
  const released = Promise.withResolvers();
  let disposed = 0;
  const rig = fixture(t, async () => {
    entered.resolve();
    try {
      await released.promise;
      return { root, indexed: false };
    } finally {
      disposed++;
    }
  });
  const search = outcome(rig.backend.search(ordinaryInput()));
  t.after(async () => {
    released.resolve();
    await search;
    await rig.backend.close();
  });
  await entered.promise;
  let closed = false;
  const closing = rig.backend.close().then(() => {
    closed = true;
  });
  await nextTurn();
  assert.equal(closed, false);
  assert.equal(disposed, 0);
  released.resolve();
  assert.equal((await search).error.code, "DAEMON_SHUTTING_DOWN");
  await closing;
  assert.equal(disposed, 1);
  assert.equal(rig.activations(), 0);
  assert.deepEqual(rig.backend.scheduler.snapshot(), { queued: 0, running: 0 });
});

test("current-source backend preserves caller abort reason after draining metadata", async (t) => {
  const entered = Promise.withResolvers();
  const released = Promise.withResolvers();
  const rig = fixture(t, async () => {
    entered.resolve();
    await released.promise;
    return { root, indexed: false };
  });
  const controller = new AbortController();
  const reason = { cancelled: "metadata" };
  const search = outcome(
    rig.backend.search(ordinaryInput(), { signal: controller.signal }),
  );
  t.after(async () => {
    released.resolve();
    await search;
    await rig.backend.close();
  });
  await entered.promise;
  controller.abort(reason);
  released.resolve();
  assert.equal((await search).error, reason);
  assert.equal(rig.activations(), 0);
  assert.deepEqual(rig.backend.scheduler.snapshot(), { queued: 0, running: 0 });
});

test("current-source backend cancellation wins over a late metadata failure", async (t) => {
  const entered = Promise.withResolvers();
  const released = Promise.withResolvers();
  const rig = fixture(t, async () => {
    entered.resolve();
    await released.promise;
    throw new Error("metadata reader failed during teardown");
  });
  const controller = new AbortController();
  const reason = { cancelled: "before metadata failure" };
  const search = outcome(
    rig.backend.search(ordinaryInput(), { signal: controller.signal }),
  );
  t.after(async () => {
    released.resolve();
    await search;
    await rig.backend.close();
  });
  await entered.promise;
  controller.abort(reason);
  released.resolve();
  assert.equal((await search).error, reason);
  assert.equal(rig.activations(), 0);
});
