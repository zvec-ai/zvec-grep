import assert from "node:assert/strict";
import { getEventListeners } from "node:events";
import test from "node:test";
import { awaitWithSignal } from "../../dist/engine/utils/abort.js";

test("awaitWithSignal without a signal preserves promise and result identity", async () => {
  const value = { result: "unchanged" };
  const operation = Promise.resolve(value);
  const waiting = awaitWithSignal(operation);
  assert.equal(waiting, operation);
  assert.equal(await waiting, value);

  const failure = new Error("original rejection");
  const rejected = Promise.reject(failure);
  const failedWaiting = awaitWithSignal(rejected);
  assert.equal(failedWaiting, rejected);
  await rejectsWithIdentity(failedWaiting, failure);
});

for (const outcome of ["resolve", "reject"]) {
  test(`awaitWithSignal preserves normal ${outcome} identity and removes only its own listener`, async () => {
    const controller = new AbortController();
    const callerListener = () => {};
    controller.signal.addEventListener("abort", callerListener);
    const operation = deferred();
    const result =
      outcome === "resolve"
        ? { value: "original" }
        : new Error("original failure");
    try {
      const waiting = awaitWithSignal(operation.promise, controller.signal);
      assert.equal(getEventListeners(controller.signal, "abort").length, 2);
      const checked =
        outcome === "resolve"
          ? waiting.then((value) => assert.equal(value, result))
          : rejectsWithIdentity(waiting, result);
      operation[outcome](result);
      await checked;
      assert.deepEqual(getEventListeners(controller.signal, "abort"), [
        callerListener,
      ]);
      controller.abort(new Error("too late to change the result"));
      await checked;
    } finally {
      controller.signal.removeEventListener("abort", callerListener);
    }
  });
}

test("awaitWithSignal rejects a pending wait with the exact abort reason before its operation settles", async () => {
  const controller = new AbortController();
  const reason = { code: "fixture cancellation" };
  const operation = deferred();
  const waiting = awaitWithSignal(operation.promise, controller.signal);
  const checked = rejectsWithIdentity(waiting, reason);
  assert.equal(getEventListeners(controller.signal, "abort").length, 1);
  controller.abort(reason);
  await checked;
  assert.equal(getEventListeners(controller.signal, "abort").length, 0);
  operation.resolve({ value: "late result cannot replace cancellation" });
  await nextTurn();
  await rejectsWithIdentity(waiting, reason);
});

test("awaitWithSignal observes a late rejection even when called with a pre-aborted signal", async (t) => {
  const unhandled = captureUnhandledRejections(t);
  const controller = new AbortController();
  const reason = new Error("already cancelled");
  controller.abort(reason);
  const operation = deferred();
  const waiting = awaitWithSignal(operation.promise, controller.signal);
  await rejectsWithIdentity(waiting, reason);
  assert.equal(getEventListeners(controller.signal, "abort").length, 0);
  // Deliberately do not attach another rejection handler to the operation:
  // ownership of this late outcome is the helper's responsibility.
  operation.reject(new Error("late failure after the caller has left"));
  await nextTurn();
  assert.deepEqual(unhandled, []);
});

test("awaitWithSignal observes operation rejection after abort wins a pending wait", async (t) => {
  const unhandled = captureUnhandledRejections(t);
  const controller = new AbortController();
  const reason = new Error("cancel pending work");
  const operation = deferred();
  const waiting = awaitWithSignal(operation.promise, controller.signal);
  const checked = rejectsWithIdentity(waiting, reason);
  controller.abort(reason);
  await checked;
  operation.reject(new Error("operation still owned after cancellation"));
  await nextTurn();
  assert.deepEqual(unhandled, []);
  assert.equal(getEventListeners(controller.signal, "abort").length, 0);
});

for (const outcome of ["resolve", "reject"]) {
  test(`awaitWithSignal lets abort win a queued ${outcome} continuation without unhandled rejections`, async (t) => {
    const unhandled = captureUnhandledRejections(t);
    const controller = new AbortController();
    const reason = new Error("abort before the settled operation continuation");
    const operation = deferred();
    const waiting = awaitWithSignal(operation.promise, controller.signal);
    const checked = rejectsWithIdentity(waiting, reason);
    operation[outcome](new Error("operation settled in this turn"));
    // Settlement schedules a microtask; the synchronous abort takes priority
    // before the helper's continuation has delivered that settlement.
    controller.abort(reason);
    await checked;
    await nextTurn();
    assert.deepEqual(unhandled, []);
    assert.equal(getEventListeners(controller.signal, "abort").length, 0);
  });
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((fulfill, fail) => {
    resolve = fulfill;
    reject = fail;
  });
  return { promise, resolve, reject };
}

function rejectsWithIdentity(promise, expected) {
  return assert.rejects(promise, (actual) => {
    assert.equal(actual, expected);
    return true;
  });
}

function captureUnhandledRejections(t) {
  const unhandled = [];
  const listener = (reason) => unhandled.push(reason);
  process.on("unhandledRejection", listener);
  t.after(() => process.off("unhandledRejection", listener));
  return unhandled;
}

function nextTurn() {
  return new Promise((resolve) => setImmediate(resolve));
}
