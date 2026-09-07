import assert from "node:assert/strict";
import fs from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  DaemonInstanceLock,
  readInstanceRecord,
  serverStatus,
} from "../dist/daemon/server-controller.js";
import { shouldStopStdioBridge } from "../dist/mcp/stdio-bridge.js";

test("concurrent initial lock writes publish exactly one complete owner", async () => {
  const home = await fs.mkdtemp(join(tmpdir(), "zg-instance-acquire-"));
  const originalOpen = fs.open;
  const paused = Promise.withResolvers();
  const resume = Promise.withResolvers();
  let pauseFirstWrite = true;
  let first;
  let second;
  try {
    fs.open = async (...args) => {
      const handle = await originalOpen(...args);
      if (pauseFirstWrite && args[1] === "wx") {
        pauseFirstWrite = false;
        const originalHandleWrite = handle.writeFile.bind(handle);
        handle.writeFile = async (...writeArgs) => {
          paused.resolve();
          await resume.promise;
          return originalHandleWrite(...writeArgs);
        };
      }
      return handle;
    };
    syncBuiltinESMExports();
    first = DaemonInstanceLock.acquire(home, "http://127.0.0.1:1/mcp").then(
      (lock) => ({ lock }),
      (error) => ({ error }),
    );
    await paused.promise;
    second = await DaemonInstanceLock.acquire(home, "http://127.0.0.1:1/mcp");
    await second.markReady();
    resume.resolve();
    const outcome = await first;
    assert.match(outcome.error?.message ?? "", /already running/);
    const record = await readInstanceRecord(home);
    assert.equal(record.instanceToken, second.record.instanceToken);
    assert.equal(record.ready, true);
    assert.deepEqual(await fs.readdir(join(home, "daemon")), ["instance.lock"]);
  } finally {
    resume.resolve();
    fs.open = originalOpen;
    syncBuiltinESMExports();
    await (await first)?.lock?.release();
    await second?.release();
    await fs.rm(home, { recursive: true, force: true });
  }
});

test("markReady fails explicitly after instance ownership is lost", async () => {
  const home = await fs.mkdtemp(join(tmpdir(), "zg-instance-lost-"));
  const first = await DaemonInstanceLock.acquire(
    home,
    "http://127.0.0.1:1/mcp",
  );
  let second;
  try {
    await first.release();
    second = await DaemonInstanceLock.acquire(home, "http://127.0.0.1:1/mcp");
    await assert.rejects(first.markReady(), /no longer owns/);
    assert.equal(
      (await readInstanceRecord(home)).instanceToken,
      second.record.instanceToken,
    );
    assert.equal((await readInstanceRecord(home)).ready, false);
  } finally {
    await first.release();
    await second?.release();
    await fs.rm(home, { recursive: true, force: true });
  }
});

test("instance readiness writes never expose a truncated lock to bridge polls", async () => {
  const home = await fs.mkdtemp(join(tmpdir(), "zg-instance-race-"));
  const lock = await DaemonInstanceLock.acquire(home, "http://127.0.0.1:1/mcp");
  const originalWrite = fs.writeFile;
  const observations = [];
  try {
    const connected = await serverStatus(home);
    // Hold each actual write between truncate/create and writing the JSON.
    fs.writeFile = async (path, data, options) => {
      const handle = await fs.open(path, options.flag ?? "w", options.mode);
      try {
        observations.push({
          record: await readInstanceRecord(home),
          stop: shouldStopStdioBridge(connected, await serverStatus(home)),
        });
        await handle.writeFile(data);
      } finally {
        await handle.close();
      }
    };
    syncBuiltinESMExports();
    await lock.markReady();
    assert.equal(observations.length, 1);
    assert.equal(
      observations[0].record?.instanceToken,
      lock.record.instanceToken,
    );
    assert.equal(observations[0].stop, false);
    assert.equal((await readInstanceRecord(home)).ready, true);
    assert.deepEqual(await fs.readdir(join(home, "daemon")), ["instance.lock"]);
  } finally {
    fs.writeFile = originalWrite;
    syncBuiltinESMExports();
    await lock.release();
    await fs.rm(home, { recursive: true, force: true });
  }
});

test("ready instances do not schedule redundant disk heartbeats", async (t) => {
  const home = await fs.mkdtemp(join(tmpdir(), "zg-instance-heartbeat-"));
  const lock = await DaemonInstanceLock.acquire(home, "http://127.0.0.1:1/mcp");
  const interval = t.mock.method(globalThis, "setInterval");
  try {
    await lock.markReady();
    assert.equal(interval.mock.callCount(), 0);
  } finally {
    await lock.release();
    await fs.rm(home, { recursive: true, force: true });
  }
});

test("failed readiness replacement preserves the lock and cleans temporary files", async () => {
  const home = await fs.mkdtemp(join(tmpdir(), "zg-instance-replace-"));
  const lock = await DaemonInstanceLock.acquire(home, "http://127.0.0.1:1/mcp");
  const originalRename = fs.rename;
  try {
    const before = await readInstanceRecord(home);
    fs.rename = async () => {
      throw Object.assign(new Error("replacement denied"), { code: "EPERM" });
    };
    syncBuiltinESMExports();
    await assert.rejects(lock.markReady(), { code: "EPERM" });
    assert.deepEqual(await readInstanceRecord(home), before);
    assert.deepEqual(await fs.readdir(join(home, "daemon")), ["instance.lock"]);
    fs.rename = originalRename;
    syncBuiltinESMExports();
    await lock.markReady();
    assert.equal((await readInstanceRecord(home)).ready, true);
  } finally {
    fs.rename = originalRename;
    syncBuiltinESMExports();
    await lock.release();
    await fs.rm(home, { recursive: true, force: true });
  }
});

test("readiness publication retries transient replacement conflicts", async () => {
  const home = await fs.mkdtemp(join(tmpdir(), "zg-instance-retry-"));
  const lock = await DaemonInstanceLock.acquire(home, "http://127.0.0.1:1/mcp");
  const originalRename = fs.rename;
  let attempts = 0;
  try {
    fs.rename = async (...args) => {
      attempts += 1;
      if (attempts === 1) {
        throw Object.assign(new Error("file temporarily busy"), {
          code: "EPERM",
        });
      }
      return originalRename(...args);
    };
    syncBuiltinESMExports();
    await lock.markReady();
    assert.equal(attempts, 2);
    assert.equal((await readInstanceRecord(home)).ready, true);
  } finally {
    fs.rename = originalRename;
    syncBuiltinESMExports();
    await lock.release();
    await fs.rm(home, { recursive: true, force: true });
  }
});
