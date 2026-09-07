import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdir, mkdtemp, rm, unlink, writeFile } from "node:fs/promises";
import { hostname, tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer as createHttpServer } from "node:http";
import { createServer as createNetServer } from "node:net";
import test from "node:test";
import {
  DaemonInstanceLock,
  readInstanceRecord,
  serverStatus,
  startServer,
  stopServer,
} from "../dist/daemon/server-controller.js";

test("daemon instance lock is exclusive and owner-released", async (t) => {
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
  const server = createNetServer();
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

test("server off force stops after graceful shutdown stalls", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-controller-stop-"));
  const daemon = join(home, "daemon");
  await mkdir(daemon);
  const child = spawn(
    process.execPath,
    ["--input-type=module", "-e", "setInterval(() => {}, 1_000)"],
    { stdio: "ignore", windowsHide: true },
  );
  await new Promise((resolve, reject) => {
    child.once("spawn", resolve);
    child.once("error", reject);
  });
  const childClosed = new Promise((resolve) => child.once("close", resolve));
  const server = createHttpServer(async (request, response) => {
    if (request.url === "/healthz") {
      response.writeHead(200).end('{"status":"ok"}');
      return;
    }
    if (request.url === "/control/shutdown" && request.method === "POST") {
      await unlink(join(daemon, "instance.lock"));
      response.writeHead(202).end('{"status":"stopping"}');
      return;
    }
    response.writeHead(404).end();
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  t.after(async () => {
    child.kill();
    server.close();
    await rm(home, { recursive: true, force: true });
  });
  const address = server.address();
  assert.ok(address && typeof address === "object");
  await writeFile(
    join(daemon, "instance.lock"),
    `${JSON.stringify({
      pid: child.pid,
      hostname: hostname(),
      instanceToken: "slow-stopping-instance",
      startedAt: Date.now(),
      updatedAt: Date.now(),
      serverUrl: `http://127.0.0.1:${address.port}/mcp`,
      ready: true,
      mcpToolset: "agent",
    })}\n`,
  );

  assert.deepEqual(await stopServer(home, 100), {
    running: false,
    ready: false,
  });
  await childClosed;
});

test("server off force stops an unresponsive recorded process", async (t) => {
  const home = await mkdtemp(
    join(tmpdir(), "zvec-grep-controller-force-stop-"),
  );
  const daemon = join(home, "daemon");
  await mkdir(daemon);
  const child = spawn(
    process.execPath,
    [
      "--input-type=module",
      "-e",
      'process.on("SIGTERM", () => {}); setInterval(() => {}, 1_000)',
    ],
    { stdio: "ignore", windowsHide: true },
  );
  await new Promise((resolve, reject) => {
    child.once("spawn", resolve);
    child.once("error", reject);
  });
  const childClosed = new Promise((resolve) => child.once("close", resolve));
  const server = createHttpServer(() => {
    // Leave every request unanswered to model a blocked daemon event loop.
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  t.after(async () => {
    if (child.exitCode === null) child.kill("SIGKILL");
    server.closeAllConnections?.();
    server.close();
    await rm(home, { recursive: true, force: true });
  });
  const address = server.address();
  assert.ok(address && typeof address === "object");
  await writeFile(
    join(daemon, "instance.lock"),
    `${JSON.stringify({
      pid: child.pid,
      hostname: hostname(),
      instanceToken: "unresponsive-instance",
      startedAt: Date.now(),
      updatedAt: Date.now(),
      serverUrl: `http://127.0.0.1:${address.port}/mcp`,
      ready: true,
      mcpToolset: "agent",
    })}\n`,
  );

  assert.deepEqual(await stopServer(home, 200), {
    running: false,
    ready: false,
  });
  await childClosed;
  assert.equal(child.exitCode === null && child.signalCode === null, false);
});

test("server on reports an occupied listen address without timing out", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-controller-port-"));
  const server = createNetServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  t.after(async () => {
    server.close();
    await rm(home, { recursive: true, force: true });
  });
  const address = server.address();
  assert.ok(address && typeof address === "object");

  await assert.rejects(
    startServer({
      cliPath: fileURLToPath(new URL("../dist/cli/index.js", import.meta.url)),
      listen: `127.0.0.1:${address.port}`,
      home,
      timeoutMs: 200,
    }),
    /address.*already in use|already.*listening/i,
  );
});

test("concurrent server starts converge on one daemon", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-controller-race-"));
  const port = await availablePort();
  t.after(async () => {
    await stopServer(home).catch(() => undefined);
    await rm(home, { recursive: true, force: true });
  });
  const options = {
    cliPath: fileURLToPath(new URL("../dist/cli/index.js", import.meta.url)),
    listen: `127.0.0.1:${port}`,
    home,
  };

  const [first, second] = await Promise.all([
    startServer(options),
    startServer(options),
  ]);
  assert.equal(first.ready, true);
  assert.equal(second.ready, true);
  assert.equal(first.pid, second.pid);
});

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
