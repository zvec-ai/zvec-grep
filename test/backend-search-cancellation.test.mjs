import assert from "node:assert/strict";
import { mkdir, realpath, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { DaemonBackend } from "../dist/daemon/backend.js";
import { daemonLeasePath } from "../dist/engine/utils/daemon-lease.js";
import { createZvecGrep } from "../dist/index.js";
import { FakeEmbeddingModel } from "./helpers/fake-embedding.mjs";
import {
  createTemporaryDirectory,
  removeTemporaryDirectory,
} from "./helpers/fixtures.mjs";

test("cancelling one initial-probe waiter returns promptly and preserves the shared probe for another search", async (t) => {
  const rig = await fixture(t);
  const controller = new AbortController();
  const reason = new Error("cancel first initial-probe waiter");
  const first = rig.observe(
    rig.backend.search(hybridInput(rig.root), { signal: controller.signal }),
  );
  await bounded(rig.entered.promise, "initial status probe did not enter");
  controller.abort(reason);
  const rejected = assert.rejects(first, (error) => error === reason);
  await bounded(rejected, "aborted search kept waiting for the initial scan");
  assert.equal(
    rig.probeClosed(),
    0,
    "cancelling a waiter cannot close its shared probe",
  );
  assert.equal(rig.modelLoads(), 0);
  assert.deepEqual(rig.backend.scheduler.snapshot(), { queued: 0, running: 0 });
  assert.equal(rig.backend.scheduler.getByRoot(rig.root), undefined);
  const runtime = rig.backend.runtimeManager.getByCanonicalRoot(rig.root);
  assert.ok(runtime);
  assert.equal(
    runtime.snapshot().activeOperations,
    1,
    "the actual pending scan retains runtime activity",
  );

  let secondSettled = false;
  const second = rig.observe(
    rig.backend.search(ftsInput(rig.root)).finally(() => {
      secondSettled = true;
    }),
  );
  await until(
    () => runtime.snapshot().activeOperations === 2,
    "second waiter did not join the active probe",
  );
  assert.equal(rig.probeOpened(), 1, "initial freshness is single-flight");
  assert.equal(secondSettled, false);
  assert.equal(rig.modelLoads(), 0);
  rig.released.resolve();
  const response = await bounded(
    second,
    "shared probe did not release the surviving waiter",
  );
  assert.equal(response.freshness, "fresh");
  assert.ok(
    response.result.items.some((item) => item.content.includes("PROBE_SOURCE")),
  );
  assert.equal(rig.probeOpened(), 1);
  assert.equal(rig.probeClosed(), 1);
  assert.equal(runtime.snapshot().activeOperations, 0);
  assert.equal(
    rig.modelLoads(),
    0,
    "the cancelled primary query must never embed later",
  );
  assert.equal(rig.backend.scheduler.getByRoot(rig.root), undefined);
});

test("shutdown drains an initial probe after its only waiter cancels before releasing daemon root ownership", async (t) => {
  const rig = await fixture(t);
  const controller = new AbortController();
  const reason = new Error("cancel lone initial-probe waiter");
  const search = rig.observe(
    rig.backend.search(hybridInput(rig.root), { signal: controller.signal }),
  );
  await bounded(rig.entered.promise, "initial status probe did not enter");
  controller.abort(reason);
  await bounded(
    assert.rejects(search, (error) => error === reason),
    "aborted search kept waiting for the initial scan",
  );
  const runtime = rig.backend.runtimeManager.getByCanonicalRoot(rig.root);
  assert.ok(runtime);
  assert.equal(runtime.snapshot().activeOperations, 1);
  assert.equal(runtime.needsReconciliation(), true);
  await stat(daemonLeasePath(rig.root));
  let closeSettled = false;
  const closing = rig.observe(
    rig.backend.close().then(() => {
      closeSettled = true;
    }),
  );
  // Allow the known watcher/scheduler shutdown microtasks to reach the pending
  // probe drain. No timer or polling is responsible for releasing this gate.
  await new Promise((resolve) => setImmediate(resolve));
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(closeSettled, false);
  assert.equal(rig.probeClosed(), 0);
  await stat(daemonLeasePath(rig.root));
  rig.released.resolve();
  await bounded(closing, "shutdown did not drain the released status scan");
  assert.equal(
    rig.probeClosed(),
    1,
    "the probe service closes through its finally path",
  );
  assert.equal(runtime.snapshot().activeOperations, 0);
  assert.equal(
    runtime.needsReconciliation(),
    true,
    "a late probe cannot commit fresh state after runtime close",
  );
  assert.equal(rig.modelLoads(), 0);
  assert.equal(rig.backend.scheduler.getByRoot(rig.root), undefined);
  await assert.rejects(stat(daemonLeasePath(rig.root)), { code: "ENOENT" });
});

async function fixture(t) {
  const temporary = await createTemporaryDirectory(
    t,
    "zvec-probe-cancellation-",
    { cleanup: false },
  );
  const entered = deferred();
  const released = deferred();
  const pending = [];
  let backend;
  t.after(async () => {
    released.resolve();
    try {
      await bounded(
        Promise.allSettled(pending),
        "pending test searches did not drain",
      );
    } finally {
      try {
        if (backend)
          await bounded(backend.close(), "test backend did not close");
      } finally {
        await removeTemporaryDirectory(temporary);
      }
    }
  });
  const path = join(temporary, "repo");
  await mkdir(path);
  const root = await realpath(path);
  await writeFile(
    join(root, "pool.ts"),
    "export function connectionPool() { return 'PROBE_SOURCE'; }\n",
  );
  const seed = await createZvecGrep({
    root,
    embeddingModel: new FakeEmbeddingModel(),
  });
  try {
    await seed.index();
  } finally {
    await seed.close();
  }
  let probeOpened = 0;
  let probeClosed = 0;
  let modelLoads = 0;
  backend = new DaemonBackend({
    version: "1.0.0",
    runtimeIdleTtlMs: 60_000,
    watchManagerFactory: () => ({
      start() {},
      flushPending: async () => {},
      close: async () => {},
    }),
    modelPoolOptions: {
      createModel: () => {
        modelLoads += 1;
        return new FakeEmbeddingModel();
      },
    },
    inspectRoot: async (requestedRoot, options, includeStatus) => {
      // Retain a real model-free service across the synthetic slow status-I/O
      // boundary. Its underlying info() and read-lock cleanup remain real.
      const service = await createZvecGrep({ ...options, root: requestedRoot });
      try {
        if (includeStatus) {
          probeOpened += 1;
          entered.resolve();
          await released.promise;
        }
        return await service.info({ root: requestedRoot, includeStatus });
      } finally {
        await service.close();
        if (includeStatus) probeClosed += 1;
      }
    },
  });
  return {
    root,
    backend,
    entered,
    released,
    probeOpened: () => probeOpened,
    probeClosed: () => probeClosed,
    modelLoads: () => modelLoads,
    observe(promise) {
      promise.catch(() => {});
      pending.push(promise);
      return promise;
    },
  };
}

function hybridInput(root) {
  return {
    root,
    queries: ["connectionPool"],
    routes: [],
    freshness: "eventual",
    autoUpdate: true,
  };
}

function ftsInput(root) {
  return {
    root,
    routes: [{ mode: "fts", query: "connectionPool" }],
    freshness: "eventual",
    autoUpdate: false,
  };
}

function deferred() {
  let resolve;
  const promise = new Promise((done) => {
    resolve = done;
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

async function until(check, label) {
  const deadline = performance.now() + 3_000;
  while (!check()) {
    if (performance.now() >= deadline) throw new Error(label);
    await delay(5);
  }
}
