import assert from "node:assert/strict";
import test from "node:test";
import { Model2VecWorkerPool } from "../../../dist/engine/models/backends/model2vec-worker-pool.js";

function createPool(maxWorkers = 2) {
  return new Model2VecWorkerPool(
    {
      tokenizerSource: "/unused",
      maxInputTokens: 8,
      normalize: true,
      tableBuffer: new SharedArrayBuffer(8),
      dimension: 2,
      dtype: "F32",
      rows: 1,
    },
    maxWorkers,
    {
      compute: new URL("../../fixtures/model2vec-worker.mjs", import.meta.url),
      tokenizer: new URL(
        "../../fixtures/model2vec-tokenizer-worker.mjs",
        import.meta.url,
      ),
    },
  );
}

test("Model2Vec worker pool runs simultaneous batches on separate threads", async (t) => {
  const pool = createPool(2);
  t.after(() => pool.dispose());
  await pool.start();

  const [first, second] = await Promise.all([
    pool.run(["30"]),
    pool.run(["30"]),
  ]);

  assert.equal(first.vectors.length, 1);
  assert.equal(second.vectors.length, 1);
  assert.notEqual(first.vectors[0][0], second.vectors[0][0]);
  await assert.rejects(pool.run(["error"]), {
    name: "FixtureError",
    message: "fixture worker failed",
  });
  await assert.rejects(pool.run(["malformed"]), /invalid vector buffer/);
});

test("Model2Vec worker pool propagates cancellation and disposes idempotently", async () => {
  const pool = createPool(1);
  await pool.start();
  const controller = new AbortController();
  const embedding = pool.run(["100"], controller.signal);
  controller.abort(new Error("cancelled fixture embedding"));
  await assert.rejects(embedding, /cancelled fixture embedding/);

  await pool.dispose();
  await pool.dispose();
  await assert.rejects(pool.run(["0"]), /disposed/);
});

test("Model2Vec worker pool reports worker startup failures", async () => {
  const pool = new Model2VecWorkerPool(
    {
      tokenizerSource: "/unused",
      maxInputTokens: 8,
      normalize: true,
      tableBuffer: new SharedArrayBuffer(8),
      dimension: 2,
      dtype: "F32",
      rows: 1,
    },
    1,
    {
      compute: new URL("../../fixtures/missing-worker.mjs", import.meta.url),
      tokenizer: new URL(
        "../../fixtures/model2vec-tokenizer-worker.mjs",
        import.meta.url,
      ),
    },
  );
  await assert.rejects(pool.start(), /Cannot find module/);
  await pool.dispose();
});
