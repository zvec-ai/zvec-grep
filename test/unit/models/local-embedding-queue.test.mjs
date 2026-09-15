import assert from "node:assert/strict";
import { setImmediate } from "node:timers/promises";
import test from "node:test";
import { LocalEmbeddingQueue } from "../../../dist/engine/models/local-embedding-queue.js";

test("local inference queue bounds work and drains before an exclusive operation", async () => {
  const queue = new LocalEmbeddingQueue(async () => 2);
  const first = Promise.withResolvers();
  const second = Promise.withResolvers();
  const barrier = Promise.withResolvers();
  const started = [];
  const a = queue.run(async () => {
    started.push("a");
    return await first.promise;
  });
  const b = queue.run(async () => {
    started.push("b");
    return await second.promise;
  });
  const exclusive = queue.run(async () => {
    started.push("exclusive");
    await barrier.promise;
  }, true);
  const c = queue.run(async () => started.push("c"));
  await setImmediate();
  assert.deepEqual(started, ["a", "b"]);
  first.resolve(1);
  await a;
  await setImmediate();
  assert.deepEqual(started, ["a", "b"]);
  second.resolve(2);
  await b;
  await setImmediate();
  assert.deepEqual(started, ["a", "b", "exclusive"]);
  barrier.resolve();
  await Promise.all([exclusive, c]);
  assert.deepEqual(started, ["a", "b", "exclusive", "c"]);
});

test("local inference queue returns slots after failure and retries failed initialization", async () => {
  let resolutions = 0;
  const queue = new LocalEmbeddingQueue(async () => {
    if (++resolutions === 1) throw new Error("initialization failed");
    return 1;
  });
  await Promise.all([
    assert.rejects(
      queue.run(async () => 0),
      /initialization failed/,
    ),
    assert.rejects(
      queue.run(async () => 0),
      /initialization failed/,
    ),
  ]);
  const failed = queue.run(async () => {
    throw new Error("inference failed");
  });
  const next = queue.run(async () => 42);
  await assert.rejects(failed, /inference failed/);
  assert.equal(await next, 42);
  assert.equal(resolutions, 2);
});
