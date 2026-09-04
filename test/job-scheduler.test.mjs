import assert from "node:assert/strict";
import test from "node:test";
import { DaemonError } from "../dist/daemon/errors.js";
import { JobScheduler } from "../dist/daemon/job-scheduler.js";
import { EngineError } from "../dist/engine/errors.js";

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

test("scheduler cancels queued and running jobs during close", async () => {
  const scheduler = new JobScheduler({ concurrency: 1 });
  let runningSignal;
  let runningCancelled = false;
  const running = scheduler.submit({
    canonicalRoot: "/repo-a",
    reason: "manual",
    run: (_report, signal) => {
      runningSignal = signal;
      return new Promise((resolve) => {
        signal.addEventListener(
          "abort",
          () => {
            runningCancelled = true;
            resolve();
          },
          { once: true },
        );
      });
    },
  });
  const queued = scheduler.submit({
    canonicalRoot: "/repo-b",
    reason: "manual",
    run: async () => assert.fail("queued job must not start"),
  });
  await waitFor(() => runningSignal !== undefined);
  const closing = scheduler.close();
  assert.equal((await scheduler.wait(queued.job.id)).state, "cancelled");
  await closing;
  const result = await scheduler.wait(running.job.id);
  assert.equal(runningSignal.aborted, true);
  assert.equal(runningCancelled, true);
  assert.equal(result.state, "cancelled");
});

test("scheduler preserves a running job success before close", async () => {
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
  await waitFor(() => releaseRunning !== undefined);
  releaseRunning();
  assert.equal((await scheduler.wait(running.job.id)).state, "succeeded");
  await scheduler.close();
});

test("scheduler status redacts credentials from failures", async () => {
  const scheduler = new JobScheduler({ maxAttempts: 1 });
  const submitted = scheduler.submit({
    canonicalRoot: "/repo",
    reason: "manual",
    run: async () => {
      const error = new Error(
        "provider failed api_key=top-secret token:another-secret",
      );
      error.code = "AUTH_FAILED apiKey=code-secret";
      throw error;
    },
  });
  const result = await scheduler.wait(submitted.job.id);
  assert.equal(result.state, "failed");
  assert.equal(result.error.code, "INDEX_FAILED");
  assert.match(result.error.message, /\[redacted\]/);
  assert.doesNotMatch(
    `${result.error.code}\n${result.error.message}`,
    /top-secret|another-secret|code-secret/,
  );
  await scheduler.close();
});

test("scheduler preserves bounded EngineError diagnostics without credentials", async () => {
  const scheduler = new JobScheduler({ maxAttempts: 1 });
  const submitted = scheduler.submit({
    canonicalRoot: "/repo",
    reason: "manual",
    run: async () => {
      const networkCause = new Error(
        "self-signed certificate in certificate chain token=nested-secret",
      );
      networkCause.code = "SELF_SIGNED_CERT_IN_CHAIN";
      throw new EngineError("Indexing completed with 1 failed file", {
        code: "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED",
        context:
          "failedReasons=src/a.ts: ZVEC_GREP.ENGINE.MODELS.QWEN_API_ERROR model=qwen/text-embedding-v4 status=403 providerCode=InvalidApiKey endpoint=https://user:password@example.test/embeddings?api_key=context-secret Authorization: Bearer context-bearer\n" +
          "Authorization: Basic basic-secret\ndetail=" +
          "x".repeat(5_000),
        cause: new TypeError(
          "fetch failed token=cause-secret apiKey='quoted-secret' sk-secretvalue123",
          { cause: networkCause },
        ),
      });
    },
  });

  const result = await scheduler.wait(submitted.job.id);
  assert.equal(result.state, "failed");
  assert.equal(result.error.code, "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED");
  assert.match(result.error.context, /status=403/);
  assert.match(result.error.context, /providerCode=InvalidApiKey/);
  assert.match(result.error.context, /MODEL.*QWEN_API_ERROR/);
  assert.match(result.error.context, /\[redacted\]/);
  assert.match(result.error.cause, /fetch failed/);
  assert.match(result.error.cause, /SELF_SIGNED_CERT_IN_CHAIN/);
  assert.match(result.error.cause, /self-signed certificate/);
  assert.match(result.error.cause, /\[redacted\]/);
  assert.equal(result.error.context.length, 4_096);
  assert.match(result.error.context, /…$/);
  assert.ok(result.error.cause.length <= 512);
  assert.doesNotMatch(
    `${result.error.message}\n${result.error.context}\n${result.error.cause}`,
    /context-secret|context-bearer|basic-secret|cause-secret|nested-secret|quoted-secret|secretvalue123|user:password/,
  );
  await scheduler.close();
});

test("scheduler preserves model download failure context in job snapshots", async (t) => {
  const scheduler = new JobScheduler({ maxAttempts: 1 });
  t.after(() => scheduler.close());
  const context = [
    "failedFiles=1",
    "failedReasons=[ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_DOWNLOAD_FAILED] Failed to download Model2Vec model",
    "model=local/potion-code-16m-v2",
    "downloadUrl=https://models.example/potion-code-16m-v2/tokenizer.json",
    "status=503",
  ].join("\n");
  const expectedError = {
    code: "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED",
    message: "Indexing completed with 1 failed file",
    context,
  };
  const submitted = scheduler.submit({
    canonicalRoot: "/repo-model-download",
    reason: "manual",
    run: async () => {
      throw new EngineError(expectedError.message, {
        code: expectedError.code,
        context,
      });
    },
  });

  const completed = await scheduler.wait(submitted.job.id);
  const byId = scheduler.get(submitted.job.id);
  const byRoot = scheduler.getByRoot("/repo-model-download");
  for (const result of [completed, byId, byRoot]) {
    assert.equal(result.state, "failed");
    assert.deepEqual(result.error, expectedError);
  }
  assert.notStrictEqual(completed.error, byId.error);
  assert.notStrictEqual(byId.error, byRoot.error);

  completed.error.context = "changed completion";
  byId.error.context = "changed lookup";
  byRoot.error.message = "changed root lookup";
  assert.deepEqual(scheduler.get(submitted.job.id).error, expectedError);
  assert.deepEqual(
    scheduler.getByRoot("/repo-model-download").error,
    expectedError,
  );
});

test("scheduler redacts credentials from failure context", async (t) => {
  const scheduler = new JobScheduler({ maxAttempts: 1 });
  t.after(() => scheduler.close());
  const submitted = scheduler.submit({
    canonicalRoot: "/repo-model-download",
    reason: "manual",
    run: async () => {
      throw new EngineError(
        "Indexing completed with 1 failed file secret=message-secret Bearer \"message-bearer-secret\" status=401 Basic 'message-basic-secret'",
        {
          code: "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED",
          context: [
            "failedReasons=MODEL2VEC_DOWNLOAD_FAILED",
            "model=local/potion-code-16m-v2",
            "authorization=Bearer bearer-secret",
            "apiKey=camel-secret api_key=snake-secret token:token-secret",
            '{"api_key":"json-secret","token":"quoted-token-start quoted-token-end"}',
            JSON.stringify({ password: 'escaped-prefix"escaped-tail' }),
            "secret='single-prefix\\'single-tail'",
            "Authorization: Basic basic-secret",
            "Authorization: Negotiate negotiate-secret",
            JSON.stringify({
              authorization: 'Negotiate quoted-auth-start, quoted-auth-end"',
            }),
            "Basic standalone-basic-secret",
            'Bearer "context-bearer-secret" status=403',
            "Basic 'context-basic-secret' retryable=false",
            "id_token=id-secret refresh_token=refresh-secret",
            "downloadUrl=https://models.example/tokenizer.json?token=url-secret",
            "downloadUrl=https://user-secret:password-secret@models.example/tokenizer.json?access_token=access-secret&other=retained",
            "downloadUrl=ftp://userinfo-secret@models.example/tokenizer.json",
            "downloadUrl=custom.scheme+v1://scheme-user-secret@models.example/tokenizer.json",
          ].join("\n"),
          cause: new Error(
            'fetch failed password="cause password secret" id_token=cause-id-secret Bearer "cause-bearer-secret" causeCode=AUTH Basic \'cause-basic-secret\'',
          ),
        },
      );
    },
  });

  const result = await scheduler.wait(submitted.job.id);
  assert.equal(result.state, "failed");
  assert.match(result.error.context, /MODEL2VEC_DOWNLOAD_FAILED/);
  assert.match(result.error.context, /model=local\/potion-code-16m-v2/);
  assert.match(result.error.context, /\[redacted\]/);
  assert.match(
    result.error.message,
    /Bearer \[redacted\] status=401 Basic \[redacted\]/,
  );
  assert.match(result.error.context, /Bearer \[redacted\]/);
  assert.match(result.error.context, /Basic \[redacted\]/);
  assert.match(result.error.context, /status=403/);
  assert.match(result.error.context, /retryable=false/);
  assert.match(
    result.error.cause,
    /Bearer \[redacted\] causeCode=AUTH Basic \[redacted\]/,
  );
  assert.doesNotMatch(
    JSON.stringify(result.error),
    /message-secret|message-bearer-secret|message-basic-secret|bearer-secret|camel-secret|snake-secret|token-secret|json-secret|quoted-token-start|quoted-token-end|escaped-prefix|escaped-tail|single-prefix|single-tail|basic-secret|negotiate-secret|quoted-auth-start|quoted-auth-end|standalone-basic-secret|context-bearer-secret|context-basic-secret|id-secret|refresh-secret|url-secret|user-secret|password-secret|access-secret|userinfo-secret|scheme-user-secret|cause password secret|cause-id-secret|cause-bearer-secret|cause-basic-secret/,
  );
  assert.match(result.error.context, /models\.example\/tokenizer\.json/);
  assert.match(result.error.context, /&other=retained/);
  assert.deepEqual(scheduler.get(submitted.job.id).error, result.error);
  assert.deepEqual(
    scheduler.getByRoot("/repo-model-download").error,
    result.error,
  );
});

test("scheduler bounds failure context without losing the initial cause", async (t) => {
  const scheduler = new JobScheduler({ maxAttempts: 1 });
  t.after(() => scheduler.close());
  const cause = [
    "failedReasons=MODEL2VEC_DOWNLOAD_FAILED",
    "model=local/potion-code-16m-v2",
  ].join("\n");
  const submitted = scheduler.submit({
    canonicalRoot: "/repo-long-failure",
    reason: "manual",
    run: async () => {
      throw new EngineError("Indexing completed with 1 failed file", {
        code: "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED",
        context: `${cause}\ndetail=${"x.".repeat(30_000)}\ntoken=tail-secret`,
      });
    },
  });

  const result = await scheduler.wait(submitted.job.id);
  assert.equal(result.state, "failed");
  assert.equal(result.error.context.startsWith(cause), true);
  assert.ok(result.error.context.length > 512);
  assert.ok(result.error.context.length <= 4096);
  assert.doesNotMatch(result.error.context, /tail-secret/);
});

test("scheduler keeps failures without context backward compatible", async (t) => {
  const cases = [
    {
      error: new EngineError("Indexing failed", {
        code: "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED",
      }),
      expected: {
        code: "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED",
        message: "Indexing failed",
      },
    },
    {
      error: new DaemonError("INDEX_BUSY", "busy", false),
      expected: { code: "INDEX_BUSY", message: "[INDEX_BUSY] busy" },
    },
    {
      error: new Error("Indexing failed"),
      expected: { code: "INDEX_FAILED", message: "Indexing failed" },
    },
  ];
  for (const { error, expected } of cases) {
    await t.test(error.name, async (t) => {
      const scheduler = new JobScheduler({ maxAttempts: 1 });
      t.after(() => scheduler.close());
      const submitted = scheduler.submit({
        canonicalRoot: "/repo",
        reason: "manual",
        run: async () => {
          throw error;
        },
      });
      const result = await scheduler.wait(submitted.job.id);
      assert.equal(result.state, "failed");
      assert.deepEqual(result.error, expected);
    });
  }
});

test("scheduler releases finished run closures and bounds retained jobs", async (t) => {
  const scheduler = new JobScheduler({ concurrency: 1 });
  t.after(() => scheduler.close());
  const submitted = [];
  for (let index = 0; index < 300; index++) {
    submitted.push(
      scheduler.submit({
        canonicalRoot: `/repo-${index}`,
        reason: "manual",
        run: async () => {},
      }).job,
    );
  }
  const finished = [];
  for (const job of submitted) {
    finished.push(await scheduler.wait(job.id));
  }
  assert.ok(
    finished.every((job) => job.state === "succeeded"),
    "every job should succeed",
  );

  // Terminal jobs keep their snapshots for late lookups but must not pin the
  // run closure (which can capture credentials from the index options).
  const newest = submitted[submitted.length - 1];
  const internals = scheduler;
  assert.equal(internals.jobs.get(newest.id)?.run, undefined);
  assert.equal(scheduler.get(newest.id)?.state, "succeeded");
  assert.equal(scheduler.getByRoot(newest.canonicalRoot)?.state, "succeeded");

  // The jobs Map stays bounded; the oldest finished jobs are evicted.
  assert.equal(internals.jobs.size, 256);
  assert.equal(scheduler.get(submitted[0].id), undefined);
  assert.equal((await scheduler.wait(newest.id)).state, "succeeded");
});

test("scheduler clears the run reference once a job reaches a terminal state", async (t) => {
  const scheduler = new JobScheduler({
    concurrency: 1,
    maxAttempts: 2,
    retryBaseDelayMs: 5,
  });
  t.after(() => scheduler.close());
  let attempts = 0;
  const retrying = scheduler.submit({
    canonicalRoot: "/repo-retry",
    reason: "manual",
    run: async () => {
      attempts += 1;
      if (attempts === 1) {
        throw new DaemonError("INDEX_BUSY", "busy", true);
      }
    },
  });
  assert.equal((await scheduler.wait(retrying.job.id)).state, "succeeded");
  assert.equal(attempts, 2);
  const internals = scheduler;
  assert.equal(internals.jobs.get(retrying.job.id)?.run, undefined);
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
