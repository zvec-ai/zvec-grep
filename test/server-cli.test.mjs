import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { Client } from "@modelcontextprotocol/client";
import { StdioClientTransport } from "@modelcontextprotocol/client/stdio";
import { parseArgs } from "../dist/cli/args.js";
import {
  resolveDirectSearchPolicy,
  resolveServerSearchPolicy,
} from "../dist/client/search-policy.js";
import { DaemonClient } from "../dist/client/daemon-client.js";
import { parseListenAddress } from "../dist/daemon/config.js";
import { DaemonHttpServer } from "../dist/daemon/http-server.js";
import { readInstanceRecord } from "../dist/daemon/server-controller.js";

const execFileAsync = promisify(execFile);
const cliPath = resolve("dist/cli/index.js");
const packageVersion = JSON.parse(
  await readFile(new URL("../package.json", import.meta.url), "utf8"),
).version;

test("server run parses a loopback listen address", () => {
  const parsed = parseArgs(["--server", "run", "--listen", "127.0.0.1:8123"]);
  assert.equal(parsed.command, "server");
  assert.equal(parsed.options.serverAction, "run");
  assert.equal(parsed.options.listen, "127.0.0.1:8123");
  assert.deepEqual(parseListenAddress(parsed.options.listen), {
    host: "127.0.0.1",
    port: 8123,
  });
});

test("server lifecycle and client mode arguments are parsed", () => {
  assert.equal(parseArgs(["--server", "--stdio"]).options.serverStdio, true);
  assert.throws(
    () => parseArgs(["--server", "on", "--stdio"]),
    /cannot be combined/i,
  );
  for (const action of ["on", "off", "status"]) {
    const parsed = parseArgs(["--server", action]);
    assert.equal(parsed.options.serverAction, action);
  }
  assert.equal(
    parseArgs(["--server", "on", "--mcp-toolset", "full"]).options.mcpToolset,
    "full",
  );
  assert.equal(
    parseArgs(["--server", "run", "--mcp-toolset=agent"]).options.mcpToolset,
    "agent",
  );
  assert.throws(
    () => parseArgs(["--server", "on", "--mcp-toolset", "all"]),
    /agent.*full/i,
  );
  assert.throws(
    () => parseArgs(["--server", "off", "--mcp-toolset", "full"]),
    /only be used with zg --server on or run/i,
  );
  assert.equal(parseArgs(["--mode", "server", "query"]).options.mode, "server");
  assert.equal(parseArgs(["--mode=auto", "query"]).options.mode, "auto");
  assert.throws(
    () => parseArgs(["--mode", "invalid", "query"]),
    /direct, server, or auto/i,
  );
  assert.throws(
    () => parseArgs(["--force-direct", "query"]),
    /requires --mode direct/i,
  );
});

test("stdio bootstrap starts and reuses the shared daemon", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-stdio-bridge-"));
  const port = await availablePort();
  t.after(async () => {
    await execFileAsync(process.execPath, [
      cliPath,
      "--server",
      "off",
      "--home",
      home,
    ]).catch(() => undefined);
    await rm(home, { recursive: true, force: true });
  });

  const client = new Client({ name: "stdio-bridge-test", version: "1.0.0" });
  const transport = new StdioClientTransport({
    command: process.execPath,
    args: [
      cliPath,
      "--server",
      "--stdio",
      "--home",
      home,
      "--listen",
      `127.0.0.1:${port}`,
    ],
    stderr: "pipe",
  });
  await client.connect(transport);
  const tools = await client.listTools();
  assert.ok(tools.tools.some((tool) => tool.name === "zvec_grep_search"));
  const daemon = await readInstanceRecord(home);
  assert.ok(daemon?.pid);

  await client.close();
  await new Promise((resolve) => setTimeout(resolve, 100));
  assert.equal((await readInstanceRecord(home))?.pid, daemon.pid);
});

test("server queries map refresh modes to search policy", () => {
  assert.deepEqual(resolveServerSearchPolicy({}), {
    freshness: "eventual",
    autoUpdate: true,
  });
  assert.deepEqual(resolveServerSearchPolicy({ refresh: "wait" }), {
    freshness: "wait_for_fresh",
    autoUpdate: true,
  });
  assert.deepEqual(resolveServerSearchPolicy({ refresh: "off" }), {
    freshness: "eventual",
    autoUpdate: false,
  });
  assert.equal(
    parseArgs(["--refresh", "background", "query"]).options.refresh,
    "background",
  );
  assert.equal(parseArgs(["--refresh=wait", "query"]).options.refresh, "wait");
  assert.throws(
    () => parseArgs(["--refresh", "invalid", "query"]),
    /background, wait, or off/i,
  );
  assert.throws(() => parseArgs(["--fresh", "query"]), /Unknown option/);
  assert.throws(
    () => parseArgs(["--no-auto-update", "query"]),
    /Unknown option/,
  );
});

test("direct queries only refresh when wait is requested", () => {
  assert.deepEqual(resolveDirectSearchPolicy({}), {
    freshness: "eventual",
    autoUpdate: false,
  });
  assert.deepEqual(resolveDirectSearchPolicy({ refresh: "background" }), {
    freshness: "eventual",
    autoUpdate: false,
  });
  assert.deepEqual(resolveDirectSearchPolicy({ refresh: "wait" }), {
    freshness: "wait_for_fresh",
    autoUpdate: true,
  });
});

test("server run rejects non-loopback addresses and unrelated listen flags", () => {
  assert.throws(() => parseListenAddress("0.0.0.0:7999"), /loopback/i);
  assert.throws(
    () =>
      new DaemonHttpServer({
        host: "0.0.0.0",
        port: 7999,
        token: "token-at-least-32-characters-long",
        version: "1.0.0",
        backend: {},
      }),
    /loopback/i,
  );
  assert.throws(
    () => parseArgs(["--listen", "127.0.0.1:7999", "query"]),
    /zg --server on or run/i,
  );
});

test("server on, status and off are idempotent", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-server-cli-"));
  const port = await availablePort();
  const args = ["--home", home];
  t.after(async () => {
    await execFileAsync(process.execPath, [
      cliPath,
      "--server",
      "off",
      ...args,
    ]).catch(() => undefined);
    await rm(home, { recursive: true, force: true });
  });

  const first = await execFileAsync(process.execPath, [
    cliPath,
    "--server",
    "on",
    "--listen",
    `127.0.0.1:${port}`,
    ...args,
  ]);
  assert.match(first.stdout, /Server: ready/);
  assert.equal((await readInstanceRecord(home)).mcpToolset, "agent");
  const second = await execFileAsync(process.execPath, [
    cliPath,
    "--server",
    "on",
    ...args,
  ]);
  assert.match(second.stdout, /Server: ready/);
  const status = await execFileAsync(process.execPath, [
    cliPath,
    "--server",
    "status",
    "--check-ready",
    ...args,
  ]);
  assert.match(status.stdout, new RegExp(`127\\.0\\.0\\.1:${port}`));
  assert.match(status.stdout, /MCP toolset: agent/);
  const mcpResponse = await fetch(`http://127.0.0.1:${port}/mcp`, {
    method: "POST",
    headers: {
      Accept: "application/json, text/event-stream",
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: "2025-06-18",
        capabilities: {},
        clientInfo: { name: "server-cli-test", version: "1.0.0" },
      },
    }),
  });
  assert.equal(mcpResponse.status, 200);
  await assert.rejects(readFile(join(home, "daemon", "token"), "utf8"), {
    code: "ENOENT",
  });
  const stopped = await execFileAsync(process.execPath, [
    cliPath,
    "--server",
    "off",
    ...args,
  ]);
  assert.match(stopped.stdout, /Server: stopped/);
  const stoppedAgain = await execFileAsync(process.execPath, [
    cliPath,
    "--server",
    "off",
    ...args,
  ]);
  assert.match(stoppedAgain.stdout, /Server: stopped/);
});

test("server toolset flag overrides the environment and the environment is a fallback", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-server-toolset-"));
  const port = await availablePort();
  const env = { ...process.env, ZVEC_GREP_MCP_TOOLSET: "full" };
  const commonArgs = ["--listen", `127.0.0.1:${port}`, "--home", home];
  t.after(async () => {
    await execFileAsync(
      process.execPath,
      [cliPath, "--server", "off", "--home", home],
      { env },
    ).catch(() => undefined);
    await rm(home, { recursive: true, force: true });
  });

  await execFileAsync(
    process.execPath,
    [cliPath, "--server", "on", "--mcp-toolset", "agent", ...commonArgs],
    { env },
  );
  assert.equal((await readInstanceRecord(home)).mcpToolset, "agent");
  const agentStatus = await execFileAsync(
    process.execPath,
    [cliPath, "--server", "status", "--home", home],
    { env },
  );
  assert.match(agentStatus.stdout, /MCP toolset: agent/);

  await execFileAsync(
    process.execPath,
    [cliPath, "--server", "off", "--home", home],
    { env },
  );
  await execFileAsync(
    process.execPath,
    [cliPath, "--server", "on", ...commonArgs],
    { env },
  );
  assert.equal((await readInstanceRecord(home)).mcpToolset, "full");
  const fullStatus = await execFileAsync(
    process.execPath,
    [cliPath, "--server", "status", "--home", home],
    { env },
  );
  assert.match(fullStatus.stdout, /MCP toolset: full/);
});

test("server token file enables authentication", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-server-token-"));
  const port = await availablePort();
  const tokenFile = join(home, "server-token");
  const token = "server-cli-test-token-at-least-32-characters";
  await writeFile(tokenFile, `${token}\n`);
  t.after(async () => {
    await execFileAsync(process.execPath, [
      cliPath,
      "--server",
      "off",
      "--home",
      home,
      "--token-file",
      tokenFile,
    ]).catch(() => undefined);
    await rm(home, { recursive: true, force: true });
  });

  await execFileAsync(process.execPath, [
    cliPath,
    "--server",
    "on",
    "--listen",
    `127.0.0.1:${port}`,
    "--home",
    home,
    "--token-file",
    tokenFile,
  ]);
  const response = await fetch(`http://127.0.0.1:${port}/mcp`, {
    method: "POST",
    body: "{}",
  });
  assert.equal(response.status, 401);
  const clientStatus = await new DaemonClient({
    serverUrl: `http://127.0.0.1:${port}/mcp`,
    tokenFile,
  }).callTool("zvec_grep_server_status", {});
  assert.equal(clientStatus.version, packageVersion);
  const stopped = await execFileAsync(process.execPath, [
    cliPath,
    "--server",
    "off",
    "--home",
    home,
    "--token-file",
    tokenFile,
  ]);
  assert.match(stopped.stdout, /Server: stopped/);
});

test(
  "foreground server releases its instance record on termination",
  {
    skip:
      process.platform === "win32"
        ? "Windows uses the control endpoint instead of POSIX signals"
        : false,
  },
  async (t) => {
    const home = await mkdtemp(join(tmpdir(), "zvec-grep-server-signal-"));
    const port = await availablePort();
    const child = spawn(
      process.execPath,
      [
        "--liftoff-only",
        cliPath,
        "--server",
        "run",
        "--listen",
        `127.0.0.1:${port}`,
        "--home",
        home,
      ],
      { stdio: "ignore", windowsHide: true },
    );
    t.after(async () => {
      if (child.exitCode === null) child.kill();
      await rm(home, { recursive: true, force: true });
    });
    await waitFor(async () => (await readInstanceRecord(home))?.ready === true);
    child.kill();
    await new Promise((resolve, reject) => {
      child.once("exit", resolve);
      child.once("error", reject);
    });
    assert.equal(await readInstanceRecord(home), undefined);
  },
);

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

async function waitFor(predicate) {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (await predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error("Condition was not reached.");
}
