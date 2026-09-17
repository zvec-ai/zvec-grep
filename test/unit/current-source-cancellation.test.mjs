import assert from "node:assert/strict";
import childProcess from "node:child_process";
import { EventEmitter, getEventListeners, once } from "node:events";
import fs from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { join } from "node:path";
import { PassThrough } from "node:stream";
import test from "node:test";
import {
  runRgFileSearch,
  runRgSearch,
} from "../../dist/engine/service/lexical.js";
import { enrichLexicalItemsWithStructure } from "../../dist/engine/service/structure-enrichment.js";

const root = process.cwd();
const nextTurn = () => new Promise((resolve) => setImmediate(resolve));
const variants = [
  [
    "literal",
    (options) => runRgSearch({ root, patterns: ["needle"], ...options }),
  ],
  [
    "ranked",
    (options) =>
      runRgSearch({
        root,
        patterns: ["needle"],
        rankItem: () => 1,
        timeoutMs: 60_000,
        ...options,
      }),
  ],
  [
    "file",
    (options) => runRgFileSearch({ root, rankPath: () => 1, ...options }),
  ],
];

function mockSpawn(t) {
  const children = [];
  const started = Promise.withResolvers();
  const mocked = t.mock.method(childProcess, "spawn", (command, args) => {
    const child = new EventEmitter();
    child.stdout = new PassThrough();
    child.stderr = new PassThrough();
    child.kills = [];
    child.kill = (signal = "SIGTERM") => {
      child.kills.push(signal);
      return true;
    };
    child.command = command;
    child.args = [...args];
    children.push(child);
    started.resolve(child);
    return child;
  });
  syncBuiltinESMExports();
  t.after(() => {
    for (const child of children) {
      child.emit("close", 0, null);
      child.stdout.destroy();
      child.stderr.destroy();
    }
    mocked.mock.restore();
    syncBuiltinESMExports();
  });
  return { children, started: started.promise };
}

function outcome(promise) {
  return promise.then(
    (value) => ({ value }),
    (error) => ({ error }),
  );
}

for (const [name, run] of variants) {
  test(`${name} current-source pre-abort preserves reason without spawning`, async (t) => {
    const rig = mockSpawn(t);
    const controller = new AbortController();
    const reason = { code: "ENOENT", message: "caller cancellation" };
    controller.abort(reason);
    assert.equal(
      (
        await outcome(
          run({
            root: "/missing-current-source-root",
            signal: controller.signal,
          }),
        )
      ).error,
      reason,
    );
    assert.equal(rig.children.length, 0);
    assert.equal(getEventListeners(controller.signal, "abort").length, 0);
  });

  test(`${name} current-source abort drains only its owned child and never retries a backend`, async (t) => {
    const rig = mockSpawn(t);
    const controller = new AbortController();
    const reason = { code: "ENOENT", message: "not a missing rg backend" };
    let settled = false;
    const result = outcome(run({ signal: controller.signal })).then((value) => {
      settled = true;
      return value;
    });
    const child = await rig.started;
    const other = outcome(run({}));
    await nextTurn();
    assert.equal(rig.children.length, 2);
    controller.abort(reason);
    await nextTurn();
    assert.deepEqual(child.kills, ["SIGKILL"]);
    assert.deepEqual(rig.children[1].kills, []);
    assert.equal(settled, false, "abort must wait for the owned close event");
    child.emit(
      "error",
      Object.assign(new Error("spawn teardown"), { code: "ENOENT" }),
    );
    child.emit("close", null, "SIGKILL");
    assert.equal((await result).error, reason);
    assert.equal(
      rig.children.length,
      2,
      "abort must not start a fallback backend",
    );
    assert.equal(getEventListeners(controller.signal, "abort").length, 0);
    rig.children[1].emit("close", 0, null);
    assert.ok((await other).value);
  });

  test(`${name} current-source success removes its abort listener`, async (t) => {
    const rig = mockSpawn(t);
    const controller = new AbortController();
    const result = run({ signal: controller.signal });
    const child = await rig.started;
    child.emit("close", 0, null);
    await result;
    assert.equal(getEventListeners(controller.signal, "abort").length, 0);
    controller.abort();
    assert.deepEqual(child.kills, []);
  });
}

test("ranked current-source abort overrides timeout truncation and clears the timer", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const rig = mockSpawn(t);
  const controller = new AbortController();
  const reason = "exact cancellation sentinel";
  const result = outcome(
    runRgSearch({
      root,
      patterns: ["needle"],
      timeoutMs: 100,
      signal: controller.signal,
    }),
  );
  const child = await rig.started;
  t.mock.timers.tick(100);
  assert.deepEqual(child.kills, ["SIGTERM"]);
  controller.abort(reason);
  child.emit("close", null, "SIGTERM");
  assert.equal(
    (await result).error,
    reason,
    "a timed-out child must not return truncated success after abort",
  );
  const kills = [...child.kills];
  t.mock.timers.tick(100_000);
  assert.deepEqual(child.kills, kills);
});

test("ranked current-source abort clears a pending timeout while child close is held", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const rig = mockSpawn(t);
  const controller = new AbortController();
  const result = outcome(
    runRgSearch({
      root,
      patterns: ["needle"],
      timeoutMs: 100,
      signal: controller.signal,
    }),
  );
  const child = await rig.started;
  controller.abort("stop before timeout");
  assert.deepEqual(child.kills, ["SIGKILL"]);
  t.mock.timers.tick(1_000);
  assert.deepEqual(
    child.kills,
    ["SIGKILL"],
    "a canceled invocation must clear its timeout before close",
  );
  child.emit("close", null, "SIGKILL");
  assert.equal((await result).error, "stop before timeout");
});

test("ranked current-source cancellation from a ranker stops later anchors", async (t) => {
  const rig = mockSpawn(t);
  const controller = new AbortController();
  const reason = new Error("stop between anchors");
  let calls = 0;
  const result = outcome(
    runRgSearch({
      root,
      patterns: ["needle"],
      signal: controller.signal,
      matchAllOccurrences: true,
      rankItem: () => {
        calls++;
        controller.abort(reason);
        return 1;
      },
    }),
  );
  const child = await rig.started;
  child.stdout.write(
    `${JSON.stringify({
      type: "match",
      data: {
        path: { text: "source.ts" },
        lines: { text: "needle needle\n" },
        line_number: 1,
        submatches: [
          { start: 0, end: 6 },
          { start: 7, end: 13 },
        ],
      },
    })}\n`,
  );
  child.emit("close", null, "SIGKILL");
  assert.equal((await result).error, reason);
  assert.equal(calls, 1);
});

test("current-source text search snapshots caller arrays before backend lookup", async (t) => {
  const rig = mockSpawn(t);
  const patterns = ["original"];
  const globs = ["*.ts"];
  const extraArgs = ["--fixed-strings"];
  const options = { root, patterns, globs, rgOptions: { extraArgs } };
  const result = runRgSearch(options);
  patterns[0] = "mutated";
  globs[0] = "*.md";
  extraArgs[0] = "--invalid-mutated-flag";
  const child = await rig.started;
  assert.ok(child.args.includes("original"));
  assert.ok(child.args.includes("*.ts"));
  assert.ok(child.args.includes("--fixed-strings"));
  assert.ok(!child.args.some((arg) => arg.includes("mutated")));
  child.emit("close", 0, null);
  await result;
});

test("active current-source cancellation terminates and drains a real owned process", async (t) => {
  const spawn = childProcess.spawn;
  const started = Promise.withResolvers();
  const children = [];
  const mocked = t.mock.method(childProcess, "spawn", () => {
    const child = spawn(
      process.execPath,
      ["-e", "process.stdout.write('ready\\n'); setInterval(() => {}, 1000)"],
      { stdio: ["ignore", "pipe", "pipe"] },
    );
    children.push(child);
    child.stdout.once("data", () => started.resolve(child));
    return child;
  });
  syncBuiltinESMExports();
  t.after(async () => {
    for (const child of children) {
      if (child.exitCode === null && child.signalCode === null) {
        const closed = once(child, "close");
        child.kill("SIGKILL");
        await closed;
      }
    }
    mocked.mock.restore();
    syncBuiltinESMExports();
  });
  const controller = new AbortController();
  const reason = new Error("real process cancelled");
  const result = outcome(
    runRgSearch({ root, patterns: ["needle"], signal: controller.signal }),
  );
  const child = await started.promise;
  let closed = false;
  child.once("close", () => {
    closed = true;
  });
  controller.abort(reason);
  assert.equal((await result).error, reason);
  assert.equal(closed, true);
  assert.equal(children.length, 1);
  assert.equal(child.signalCode, "SIGKILL");
  assert.equal(getEventListeners(controller.signal, "abort").length, 0);
});

function item(name) {
  return {
    kind: "lexical_match",
    rank: 1,
    file: {
      absolutePath: join(root, name),
      relativePath: name,
      rootPath: root,
    },
    range: {
      kind: "text",
      startLine: 1,
      endLine: 1,
      startOffset: 0,
      endOffset: 6,
    },
    content: "needle",
    status: "fresh",
    matchedBy: "lexical",
  };
}

function mockFs(t, methods) {
  const mocks = Object.entries(methods).map(([name, fn]) =>
    t.mock.method(fs, name, fn),
  );
  syncBuiltinESMExports();
  t.after(() => {
    for (const mocked of mocks) mocked.mock.restore();
    syncBuiltinESMExports();
  });
}

test("structure enrichment pre-abort skips all filesystem work", async (t) => {
  let calls = 0;
  mockFs(t, {
    stat: async () => {
      calls++;
      throw new Error("must not stat");
    },
  });
  const controller = new AbortController();
  const reason = { cancelled: true };
  controller.abort(reason);
  const result = await outcome(
    enrichLexicalItemsWithStructure(
      root,
      [item("first.ts")],
      undefined,
      undefined,
      false,
      { signal: controller.signal },
    ),
  );
  assert.equal(result.error, reason);
  assert.equal(calls, 0);
});

test("structure enrichment checks cancellation after stat before reading or parsing another file", async (t) => {
  const started = Promise.withResolvers();
  const released = Promise.withResolvers();
  let stats = 0;
  let reads = 0;
  mockFs(t, {
    stat: async () => {
      stats++;
      started.resolve();
      return released.promise;
    },
    readFile: async () => {
      reads++;
      throw new Error("must not read");
    },
  });
  t.after(() => released.resolve({ isFile: () => true, size: 6, mtimeMs: 1 }));
  const controller = new AbortController();
  const reason = "cancel after stat";
  const result = outcome(
    enrichLexicalItemsWithStructure(
      root,
      [item("first.ts"), item("second.ts")],
      undefined,
      undefined,
      false,
      { signal: controller.signal },
    ),
  );
  await started.promise;
  controller.abort(reason);
  released.resolve({ isFile: () => true, size: 6, mtimeMs: 1 });
  assert.equal((await result).error, reason);
  assert.equal(stats, 1);
  assert.equal(reads, 0);
});

test("structure enrichment closes its descriptor after cancellation during a bounded read", async (t) => {
  const started = Promise.withResolvers();
  const released = Promise.withResolvers();
  let closes = 0;
  let stats = 0;
  const info = { isFile: () => true, size: 6, mtimeMs: 1 };
  mockFs(t, {
    stat: async () => {
      stats++;
      return info;
    },
    open: async () => ({
      stat: async () => info,
      read: async () => {
        started.resolve();
        return released.promise;
      },
      close: async () => {
        closes++;
      },
    }),
  });
  t.after(() => released.resolve({ bytesRead: 6 }));
  const controller = new AbortController();
  const reason = new Error("cancel bounded read");
  const result = outcome(
    enrichLexicalItemsWithStructure(
      root,
      [item("first.ts"), item("second.ts")],
      undefined,
      32,
      true,
      { signal: controller.signal },
    ),
  );
  await started.promise;
  controller.abort(reason);
  released.resolve({ bytesRead: 6 });
  assert.equal((await result).error, reason);
  assert.equal(closes, 1);
  assert.equal(stats, 1);
});
