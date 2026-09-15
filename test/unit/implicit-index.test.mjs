import assert from "node:assert/strict";
import test from "node:test";
import {
  hasUsableImplicitIndex,
  implicitPreparationModel,
  waitForImplicitIndex,
} from "../../dist/client/implicit-index.js";
import { DaemonCallTimeoutError } from "../../dist/client/daemon-client.js";

function fakeClock() {
  let time = 0;
  return {
    now: () => time,
    delay: async (ms) => {
      time += ms;
    },
  };
}

test("implicit retries preserve the stored model and never select a remote provider", () => {
  const status = (embedding) => ({
    persistent: { workspace_index: { embedding } },
  });
  const fallback = "local/potion-code-16m-v2";
  assert.equal(implicitPreparationModel({}, fallback), fallback);
  assert.equal(implicitPreparationModel(status(null), fallback), fallback);
  assert.equal(
    implicitPreparationModel(
      status({ provider: "local", model: "multilingual-e5-small" }),
      fallback,
    ),
    "local/multilingual-e5-small",
  );
  assert.equal(
    implicitPreparationModel(
      status({ provider: "qwen", model: "text-embedding-v4" }),
      fallback,
    ),
    undefined,
  );
});

test("implicit preflight distinguishes failed initialization from a usable stale or empty index", () => {
  const initialized = {
    indexed: true,
    persistent: { files: { indexed: 0, pending: 0, failed: 1, added: 0 } },
    runtime: { job_state: "failed" },
  };
  assert.equal(hasUsableImplicitIndex(initialized), false);
  for (const job_state of ["queued", "running", "failed", "cancelled"]) {
    assert.equal(
      hasUsableImplicitIndex({ indexed: true, runtime: { job_state } }),
      false,
    );
    assert.equal(
      hasUsableImplicitIndex({
        ...initialized,
        persistent: { files: { indexed: 1, pending: 1, failed: 1, added: 0 } },
        runtime: { job_state },
      }),
      true,
    );
  }
  assert.equal(
    hasUsableImplicitIndex({
      indexed: true,
      persistent: { files: { indexed: 0, pending: 0, failed: 0, added: 0 } },
      runtime: { job_state: "succeeded" },
    }),
    true,
  );
  assert.equal(hasUsableImplicitIndex({ indexed: false }), false);
  assert.equal(
    hasUsableImplicitIndex({ indexed: true, index_policy: "disabled" }),
    false,
  );
});

test("implicit readiness returns when a small index becomes available within one shared budget", async () => {
  const budgets = [];
  const progress = [];
  const clock = fakeClock();
  const state = await waitForImplicitIndex(
    {
      budgetMs: 500,
      status: async (budget) => {
        budgets.push(budget);
        return { indexed: budgets.length === 3 };
      },
      onProgress: (status) => progress.push(status),
    },
    clock,
  );
  assert.equal(state, "ready");
  assert.deepEqual(budgets, [500, 300, 100]);
  assert.equal(progress.length, 2);
});

test("implicit readiness bounds pending jobs and does not restart failed or disabled work", async () => {
  assert.equal(
    await waitForImplicitIndex(
      {
        budgetMs: 250,
        status: async () => ({
          indexed: true,
          persistent: { files: { indexed: 0, failed: 1 } },
          runtime: { job_state: "succeeded" },
        }),
      },
      fakeClock(),
    ),
    "failed",
  );
  assert.equal(
    await waitForImplicitIndex(
      { budgetMs: 250, status: async () => ({ indexed: false }) },
      fakeClock(),
    ),
    "pending",
  );
  for (const job_state of ["failed", "cancelled"]) {
    assert.equal(
      await waitForImplicitIndex(
        {
          budgetMs: 250,
          status: async () => ({ indexed: true, runtime: { job_state } }),
        },
        fakeClock(),
      ),
      "failed",
    );
  }
  for (const job_state of ["queued", "running"]) {
    assert.equal(
      await waitForImplicitIndex(
        {
          budgetMs: 250,
          status: async () => ({ indexed: true, runtime: { job_state } }),
        },
        fakeClock(),
      ),
      "pending",
    );
  }
  assert.equal(
    await waitForImplicitIndex(
      { budgetMs: 250, status: async () => ({ index_policy: "disabled" }) },
      fakeClock(),
    ),
    "disabled",
  );
  assert.equal(
    await waitForImplicitIndex(
      {
        budgetMs: 250,
        status: async () => {
          throw new DaemonCallTimeoutError();
        },
      },
      fakeClock(),
    ),
    "pending",
  );
  await assert.rejects(
    waitForImplicitIndex(
      {
        budgetMs: 250,
        status: async () => {
          throw new Error("authentication denied");
        },
      },
      fakeClock(),
    ),
    /authentication denied/,
  );
});
