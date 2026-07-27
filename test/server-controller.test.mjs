import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { hostname, tmpdir } from "node:os";
import { join } from "node:path";
import { createServer } from "node:net";
import test from "node:test";
import {
  DaemonInstanceLock,
  readInstanceRecord,
  serverStatus,
} from "../dist/daemon/server-controller.js";

test("daemon instance lock is exclusive, heartbeat-safe and owner-released", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-controller-"));
  t.after(async () => rm(home, { recursive: true, force: true }));
  const port = await availablePort();
  const serverUrl = `http://127.0.0.1:${port}/mcp`;
  const lock = await DaemonInstanceLock.acquire(home, serverUrl);
  t.after(async () => lock.release());
  const record = await readInstanceRecord(home);
  assert.equal(record.pid, process.pid);
  assert.equal(record.ready, false);
  assert.equal(record.mcpToolset, "agent");
  await assert.rejects(
    DaemonInstanceLock.acquire(home, serverUrl),
    /already running/i,
  );
  await lock.markReady();
  assert.equal((await readInstanceRecord(home)).ready, true);
  const status = await serverStatus(home);
  assert.equal(status.running, true);
  assert.equal(status.ready, false);
  await lock.release();
  assert.equal(await readInstanceRecord(home), undefined);
});

async function availablePort() {
  const server = createServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  const port = typeof address === "object" && address ? address.port : 0;
  await new Promise((resolve, reject) =>
    server.close((error) => (error ? reject(error) : resolve())),
  );
  return port;
}

test("a dead daemon instance record is replaced", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-controller-stale-"));
  t.after(async () => rm(home, { recursive: true, force: true }));
  const daemon = join(home, "daemon");
  await mkdir(daemon);
  await writeFile(
    join(daemon, "instance.lock"),
    `${JSON.stringify({
      pid: 2_147_483_647,
      hostname: hostname(),
      instanceToken: "stale-instance",
      startedAt: 1,
      updatedAt: 1,
      serverUrl: "http://127.0.0.1:7999/mcp",
      ready: true,
    })}\n`,
  );
  const lock = await DaemonInstanceLock.acquire(
    home,
    "http://127.0.0.1:8123/mcp",
  );
  try {
    assert.equal((await readInstanceRecord(home)).pid, process.pid);
  } finally {
    await lock.release();
  }
});

test("legacy daemon records without a toolset preserve the full MCP surface", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-controller-legacy-"));
  t.after(async () => rm(home, { recursive: true, force: true }));
  const daemon = join(home, "daemon");
  const now = Date.now();
  await mkdir(daemon);
  await writeFile(
    join(daemon, "instance.lock"),
    `${JSON.stringify({
      pid: process.pid,
      hostname: hostname(),
      instanceToken: "legacy-instance",
      startedAt: now,
      updatedAt: now,
      serverUrl: "http://127.0.0.1:7999/mcp",
      ready: false,
    })}\n`,
  );

  const record = await readInstanceRecord(home);
  assert.equal(record.mcpToolset, "full");
});
