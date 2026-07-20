import assert from "node:assert/strict";
import test from "node:test";
import { DaemonError } from "../dist/daemon/errors.js";
import { JobScheduler } from "../dist/daemon/job-scheduler.js";

test("scheduler reuses same-root jobs and enforces global concurrency", async () => {
  const scheduler = new JobScheduler({ concurrency: 1 });
  let active = 0;
  let maxActive = 0;
  const releases = [];
  const run = async () => {
    active += 1;
    maxActive = Math.max(maxActive, active);
    await new Promise((resolve) => releases.push(resolve));
    active -= 1;
  };

  const first = scheduler.submit({
    canonicalRoot: "/repo-a",
    reason: "manual",
    run,
  });
  const duplicate = scheduler.submit({
    canonicalRoot: "/repo-a",
    reason: "manual",
    run,
  });
  const second = scheduler.submit({
    canonicalRoot: "/repo-b",
    reason: "manual",
    run,
  });
  assert.equal(duplicate.reused, true);
  assert.equal(duplicate.job.id, first.job.id);
  assert.equal(second.job.state, "queued");

  await waitFor(() => releases.length === 1);
  releases.shift()();
  await scheduler.wait(first.job.id);
  await waitFor(() => releases.length === 1);
  releases.shift()();
  await scheduler.wait(second.job.id);
  assert.equal(maxActive, 1);
  await scheduler.close();
});

test("scheduler retries retryable failures without blocking another root", async () => {
  const scheduler = new JobScheduler({
    concurrency: 1,
    maxAttempts: 2,
    retryBaseDelayMs: 10,
  });
  const order = [];
  let attempts = 0;
  const retrying = scheduler.submit({
    canonicalRoot: "/repo-a",
    reason: "manual",
    run: async () => {
      attempts += 1;
      order.push(`a${attempts}`);
      if (attempts === 1) {
        throw new DaemonError("INDEX_BUSY", "busy", true);
      }
    },
  });
  const other = scheduler.submit({
    canonicalRoot: "/repo-b",
    reason: "manual",
    run: async () => {
      order.push("b1");
    },
  });

  assert.equal((await scheduler.wait(other.job.id)).state, "succeeded");
  const completed = await scheduler.wait(retrying.job.id);
  assert.equal(completed.state, "succeeded");
  assert.equal(completed.attempt, 2);
  assert.equal(completed.error, undefined);
  assert.deepEqual(order, ["a1", "b1", "a2"]);
  await scheduler.close();
});

test("scheduler wait observes current and future index progress", async () => {
  const scheduler = new JobScheduler({ concurrency: 1 });
  let continueIndexing;
  const scanObserved = new Promise((resolve) => {
    continueIndexing = resolve;
  });
  const submitted = scheduler.submit({
    canonicalRoot: "/repo-progress",
    reason: "manual",
    run: async (report) => {
      report({ phase: "scanning", detail: "Scanning workspace..." });
      await scanObserved;
      report({
        phase: "indexing",
        filesIndexed: 2,
        filesTotal: 5,
        detail: "embedding src/example.ts",
      });
    },
  });
  await waitFor(
    () => scheduler.get(submitted.job.id)?.progress?.phase === "scanning",
  );

  const observed = [];
  const completed = scheduler.wait(submitted.job.id, (progress) => {
    observed.push(progress);
  });
  continueIndexing();

  assert.equal((await completed).state, "succeeded");
  assert.deepEqual(
    observed.map((progress) => progress.phase),
    ["scanning", "indexing"],
  );
  assert.equal(observed[1].filesIndexed, 2);
  assert.equal(observed[1].filesTotal, 5);
  await scheduler.close();
});

test("scheduler keeps one follow-up for changes submitted while a root is running", async () => {
  const scheduler = new JobScheduler({ concurrency: 1 });
  let releaseFirst;
  const firstStarted = new Promise((resolve) => {
    releaseFirst = resolve;
  });
  const runs = [];
  const first = scheduler.submit({
    canonicalRoot: "/repo-a",
    reason: "reconcile",
    run: async () => {
      runs.push("first");
      await firstStarted;
    },
  });
  await waitFor(() => scheduler.get(first.job.id)?.state === "running");
  const reused = scheduler.submit({
    canonicalRoot: "/repo-a",
    reason: "watch",
    run: async () => {
      runs.push("follow-up");
    },
  });
  scheduler.submit({
    canonicalRoot: "/repo-a",
    reason: "reconcile",
    run: async () => {
      runs.push("lower-priority");
    },
  });
  assert.equal(reused.reused, true);
  releaseFirst();
  await scheduler.waitForRootIdle("/repo-a");
  assert.deepEqual(runs, ["first", "follow-up"]);
  await scheduler.close();
});

test("scheduler preserves an explicit manual follow-up while a root is running", async () => {
  const scheduler = new JobScheduler({ concurrency: 1 });
  let releaseFirst;
  const runs = [];
  const first = scheduler.submit({
    canonicalRoot: "/repo-a",
    reason: "manual",
    run: () =>
      new Promise((resolve) => {
        runs.push("index");
        releaseFirst = resolve;
      }),
  });
  await waitFor(() => releaseFirst !== undefined);
  const followup = scheduler.submit({
    canonicalRoot: "/repo-a",
    reason: "manual",
    followupIfRunning: true,
    run: async () => {
      runs.push("rebuild");
    },
  });
  assert.notEqual(followup.job.id, first.job.id);
  releaseFirst();
  assert.equal((await scheduler.wait(followup.job.id)).state, "succeeded");
  assert.deepEqual(runs, ["index", "rebuild"]);
  await scheduler.close();
});

test("scheduler cancels queued jobs and lets a running job finish during close", async () => {
  const scheduler = new JobScheduler({ concurrency: 1 });
  let releaseRunning;
  const running = scheduler.submit({
    canonicalRoot: "/repo-a",
    reason: "manual",
    run: () =>
      new Promise((resolve) => {
        releaseRunning = resolve;
      }),
  });
  const queued = scheduler.submit({
    canonicalRoot: "/repo-b",
    reason: "manual",
    run: async () => assert.fail("queued job must not start"),
  });
  await waitFor(() => releaseRunning !== undefined);
  const closing = scheduler.close();
  assert.equal((await scheduler.wait(queued.job.id)).state, "cancelled");
  releaseRunning();
  await closing;
  assert.equal((await scheduler.wait(running.job.id)).state, "succeeded");
});

test("scheduler status redacts credentials from failures", async () => {
  const scheduler = new JobScheduler({ maxAttempts: 1 });
  const submitted = scheduler.submit({
    canonicalRoot: "/repo",
    reason: "manual",
    run: async () => {
      throw new Error(
        "provider failed api_key=top-secret token:another-secret",
      );
    },
  });
  const result = await scheduler.wait(submitted.job.id);
  assert.equal(result.state, "failed");
  assert.match(result.error.message, /\[redacted\]/);
  assert.doesNotMatch(result.error.message, /top-secret|another-secret/);
  await scheduler.close();
});

async function waitFor(predicate) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (predicate()) {
      return;
    }
    await new Promise((resolve) => setImmediate(resolve));
  }
  throw new Error("Condition was not reached.");
}
