import assert from "node:assert/strict";
import test from "node:test";
import { IndexCoordinator } from "../dist/daemon/index-coordinator.js";
import { DaemonError } from "../dist/daemon/errors.js";
import { JobScheduler } from "../dist/daemon/job-scheduler.js";

test("changes arriving during a write are indexed by one follow-up revision", async () => {
  const scheduler = new JobScheduler({ concurrency: 1 });
  let dirtyRevision = 0;
  let indexedRevision = 0;
  let releaseFirst;
  const firstRunning = new Promise((resolve) => {
    releaseFirst = resolve;
  });
  const snapshots = [];
  const runtime = {
    canonicalRoot: "/repo",
    markDirty: () => ++dirtyRevision,
    markIndexed: (revision) => {
      indexedRevision = revision;
    },
    setWriterPending: () => {},
    withWrite: (operation) => operation(),
  };
  const coordinator = new IndexCoordinator({
    runtime,
    scheduler,
    run: async (changes) => {
      snapshots.push(changes);
      if (snapshots.length === 1) {
        await firstRunning;
      }
    },
  });
  coordinator.enqueue(change("/repo/a.ts"));
  await waitFor(() => snapshots.length === 1);
  coordinator.enqueue(change("/repo/b.ts"));
  coordinator.enqueue(change("/repo/c.ts"));
  releaseFirst();
  await scheduler.waitForRootIdle("/repo");
  assert.equal(snapshots.length, 2);
  assert.deepEqual(snapshots[0].touchedFiles, ["/repo/a.ts"]);
  assert.deepEqual(snapshots[1].touchedFiles, ["/repo/b.ts", "/repo/c.ts"]);
  assert.equal(dirtyRevision, 3);
  assert.equal(indexedRevision, 3);
  await scheduler.close();
});

test("a retry reuses the same change snapshot", async () => {
  const scheduler = new JobScheduler({
    concurrency: 1,
    maxAttempts: 2,
    retryBaseDelayMs: 1,
  });
  let dirtyRevision = 0;
  let indexedRevision = 0;
  const snapshots = [];
  const coordinator = new IndexCoordinator({
    runtime: {
      canonicalRoot: "/repo",
      markDirty: () => ++dirtyRevision,
      markIndexed: (revision) => {
        indexedRevision = revision;
      },
      setWriterPending: () => {},
      withWrite: (operation) => operation(),
    },
    scheduler,
    run: async (changes) => {
      snapshots.push(changes);
      if (snapshots.length === 1)
        throw new DaemonError("INDEX_BUSY", "busy", true);
    },
  });
  const job = coordinator.enqueue(change("/repo/a.ts"));
  assert.equal((await scheduler.wait(job.id)).state, "succeeded");
  assert.deepEqual(
    snapshots.map((snapshot) => snapshot.touchedFiles),
    [["/repo/a.ts"], ["/repo/a.ts"]],
  );
  assert.equal(indexedRevision, 1);
  await scheduler.close();
});

test("a full reconciliation without proof remains unconfirmed", async () => {
  const scheduler = new JobScheduler({ concurrency: 1 });
  let dirtyRevision = 0;
  let indexedRevision = 0;
  let reconciledRevision = 0;
  const coordinator = new IndexCoordinator({
    runtime: {
      canonicalRoot: "/repo",
      requireFullReconciliation: () => {},
      markDirty: () => ++dirtyRevision,
      markIndexed: (revision) => {
        indexedRevision = revision;
      },
      markReconciled: (revision) => {
        reconciledRevision = revision;
      },
      setWriterPending: () => {},
      withWrite: (operation) => operation(),
    },
    scheduler,
    run: async () => undefined,
  });

  const job = coordinator.enqueue({
    touchedFiles: [],
    rescanDirectories: [],
    deletedPrefixes: [],
    forceFullReconcile: true,
  });

  assert.equal((await scheduler.wait(job.id)).state, "succeeded");
  assert.equal(indexedRevision, 1);
  assert.equal(reconciledRevision, 0);
  await scheduler.close();
});

test("a high-ratio exact batch remains incremental", async () => {
  const scheduler = new JobScheduler({ concurrency: 1 });
  let dirtyRevision = 0;
  let fullReconciliations = 0;
  const snapshots = [];
  const coordinator = new IndexCoordinator({
    runtime: {
      canonicalRoot: "/repo",
      requireFullReconciliation: () => {
        fullReconciliations += 1;
      },
      markDirty: () => ++dirtyRevision,
      markIndexed: () => {},
    },
    scheduler,
    getIndexedFileCount: () => 10,
    minRatioChangedPaths: 2,
    fullReconcileRatio: 0.2,
    run: async (changes) => {
      snapshots.push(changes);
    },
  });

  const job = coordinator.enqueue({
    touchedFiles: ["/repo/a.ts", "/repo/b.ts", "/repo/c.ts"],
    rescanDirectories: [],
    deletedPrefixes: [],
    forceFullReconcile: false,
  });

  assert.equal((await scheduler.wait(job.id)).state, "succeeded");
  assert.equal(snapshots[0].forceFullReconcile, false);
  assert.equal(fullReconciliations, 0);
  await scheduler.close();
});

test("queued exact batches compact without becoming a full reconciliation", async () => {
  const scheduler = new JobScheduler({ concurrency: 1 });
  let dirtyRevision = 0;
  let releaseFirst;
  const firstRunning = new Promise((resolve) => {
    releaseFirst = resolve;
  });
  const snapshots = [];
  const coordinator = new IndexCoordinator({
    runtime: {
      canonicalRoot: "/repo",
      requireFullReconciliation: () => {
        throw new Error("exact batches must not require full reconciliation");
      },
      markDirty: () => ++dirtyRevision,
      markIndexed: () => {},
    },
    scheduler,
    run: async (changes) => {
      snapshots.push(changes);
      if (snapshots.length === 1) await firstRunning;
    },
  });
  coordinator.enqueue(change("/repo/initial.ts"));
  await waitFor(() => snapshots.length === 1);

  coordinator.enqueue(exactBatch("/repo/package-a", 600));
  coordinator.enqueue(exactBatch("/repo/package-b", 600));
  releaseFirst();
  await scheduler.waitForRootIdle("/repo");

  assert.equal(snapshots.length, 2);
  assert.deepEqual(snapshots[1], {
    touchedFiles: [],
    rescanDirectories: ["/repo/package-a", "/repo/package-b"],
    deletedPrefixes: [],
    forceFullReconcile: false,
  });
  await scheduler.close();
});

function change(path) {
  return {
    touchedFiles: [path],
    rescanDirectories: [],
    deletedPrefixes: [],
    forceFullReconcile: false,
  };
}

function exactBatch(directory, count) {
  return {
    touchedFiles: Array.from(
      { length: count },
      (_, index) => `${directory}/${index}.ts`,
    ),
    rescanDirectories: [],
    deletedPrefixes: [],
    forceFullReconcile: false,
  };
}

async function waitFor(predicate) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (predicate()) return;
    await new Promise((resolve) => setImmediate(resolve));
  }
  throw new Error("Condition was not reached.");
}
