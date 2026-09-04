import { randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import { open, readFile, unlink } from "node:fs/promises";
import { createServer } from "node:net";
import { hostname } from "node:os";
import { join } from "node:path";
import {
  configuredListenAddress,
  daemonHome,
  resolveClientToken,
} from "./config.js";
import { processIsAlive } from "../engine/utils/daemon-lease.js";
import {
  DEFAULT_MCP_TOOLSET,
  MCP_TOOLSET_ENV,
  resolveMcpToolset,
  type McpToolset,
} from "../mcp/toolset.js";

const LIFTOFF_ONLY_FLAG = "--liftoff-only";
const SHUTDOWN_REQUEST_TIMEOUT_MS = 2_000;
const TERMINATION_GRACE_MS = 2_000;

export type DaemonInstanceRecord = {
  pid: number;
  hostname: string;
  instanceToken: string;
  startedAt: number;
  updatedAt: number;
  serverUrl: string;
  ready: boolean;
  mcpToolset: McpToolset;
};

export type DaemonControlStatus = {
  running: boolean;
  ready: boolean;
  pid?: number;
  serverUrl?: string;
  mcpToolset?: McpToolset;
};

export class DaemonInstanceLock {
  private heartbeat?: ReturnType<typeof setInterval>;

  private constructor(
    readonly path: string,
    readonly record: DaemonInstanceRecord,
  ) {}

  static async acquire(
    home: string | undefined,
    serverUrl: string,
    mcpToolset: McpToolset = DEFAULT_MCP_TOOLSET,
  ): Promise<DaemonInstanceLock> {
    const path = join(daemonHome(home), "instance.lock");
    const record: DaemonInstanceRecord = {
      pid: process.pid,
      hostname: hostname(),
      instanceToken: randomUUID(),
      startedAt: Date.now(),
      updatedAt: Date.now(),
      serverUrl,
      ready: false,
      mcpToolset,
    };
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        const handle = await open(path, "wx", 0o600);
        try {
          await handle.writeFile(`${JSON.stringify(record)}\n`);
        } finally {
          await handle.close();
        }
        return new DaemonInstanceLock(path, record);
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code === "ENOENT") {
          const { mkdir } = await import("node:fs/promises");
          await mkdir(daemonHome(home), { recursive: true, mode: 0o700 });
          continue;
        }
        if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
        const existing = await readInstanceRecord(home);
        if (
          existing &&
          existing.hostname === hostname() &&
          processIsAlive(existing.pid)
        ) {
          throw new Error(
            `zvec-grep server is already running with PID ${existing.pid}`,
            {
              cause: error,
            },
          );
        }
        await unlink(path).catch(() => undefined);
      }
    }
    throw new Error("Could not acquire the zvec-grep server instance lock.");
  }

  async markReady(): Promise<void> {
    this.record.ready = true;
    await this.write();
    this.heartbeat = setInterval(() => {
      void this.write();
    }, 5_000);
    this.heartbeat.unref?.();
  }

  async release(): Promise<void> {
    if (this.heartbeat) clearInterval(this.heartbeat);
    const current = await readRecordPath(this.path);
    if (
      current?.instanceToken === this.record.instanceToken &&
      current.pid === process.pid
    ) {
      await unlink(this.path).catch(() => undefined);
    }
  }

  private async write(): Promise<void> {
    this.record.updatedAt = Date.now();
    const current = await readRecordPath(this.path);
    if (
      current?.instanceToken !== this.record.instanceToken ||
      current.pid !== process.pid
    )
      return;
    const { writeFile } = await import("node:fs/promises");
    await writeFile(this.path, `${JSON.stringify(this.record)}\n`, {
      mode: 0o600,
    });
  }
}

export async function readInstanceRecord(
  home?: string,
): Promise<DaemonInstanceRecord | undefined> {
  return readRecordPath(join(daemonHome(home), "instance.lock"));
}

export async function serverStatus(
  home?: string,
): Promise<DaemonControlStatus> {
  const record = await readInstanceRecord(home);
  if (
    !record ||
    record.hostname !== hostname() ||
    !processIsAlive(record.pid)
  ) {
    return { running: false, ready: false };
  }
  try {
    const response = await fetch(new URL("/healthz", record.serverUrl), {
      signal: AbortSignal.timeout(1_000),
    });
    return {
      running: true,
      ready: response.ok && record.ready,
      pid: record.pid,
      serverUrl: record.serverUrl,
      mcpToolset: record.mcpToolset,
    };
  } catch {
    return {
      running: true,
      ready: false,
      pid: record.pid,
      serverUrl: record.serverUrl,
      mcpToolset: record.mcpToolset,
    };
  }
}

export async function startServer(options: {
  cliPath: string;
  listen?: string;
  tokenFile?: string;
  home?: string;
  timeoutMs?: number;
  mcpToolset?: McpToolset;
}): Promise<DaemonControlStatus> {
  const environmentToolset = process.env[MCP_TOOLSET_ENV];
  const requestedToolset = resolveMcpToolset(
    options.mcpToolset,
    environmentToolset,
  );
  const toolsetWasExplicit =
    options.mcpToolset !== undefined || environmentToolset !== undefined;
  const current = await serverStatus(options.home);
  if (current.running) {
    if (
      toolsetWasExplicit &&
      current.mcpToolset !== undefined &&
      current.mcpToolset !== requestedToolset
    ) {
      throw new Error(
        `zvec-grep server is already running with MCP toolset "${current.mcpToolset}". Run \`zg --server off\`, then restart it with \`zg --server on --mcp-toolset ${requestedToolset}\`.`,
      );
    }
    if (current.ready) return current;
    return waitForStatus(options.home, true, options.timeoutMs ?? 10_000);
  }
  const listen = configuredListenAddress(options.listen);
  try {
    await assertListenAddressAvailable(listen.host, listen.port);
  } catch (error) {
    const starting = await waitForRunningRecord(options.home, 500);
    if (starting) {
      return waitForStatus(options.home, true, options.timeoutMs ?? 10_000);
    }
    throw error;
  }
  const args = [LIFTOFF_ONLY_FLAG, options.cliPath, "--server", "run"];
  args.push("--mcp-toolset", requestedToolset);
  if (options.listen) args.push("--listen", options.listen);
  if (options.tokenFile) args.push("--token-file", options.tokenFile);
  if (options.home) args.push("--home", options.home);
  const child = spawn(process.execPath, args, {
    detached: true,
    stdio: "ignore",
    windowsHide: true,
  });
  child.unref();
  return waitForStatus(options.home, true, options.timeoutMs ?? 10_000);
}

export async function stopServer(
  home?: string,
  timeoutMs = 30_000,
  tokenFile?: string,
): Promise<DaemonControlStatus> {
  const record = await readInstanceRecord(home);
  if (
    !record ||
    record.hostname !== hostname() ||
    !processIsAlive(record.pid)
  ) {
    return { running: false, ready: false };
  }
  if (record.pid === process.pid) {
    throw new Error("Refusing to stop the current process as a daemon.");
  }
  const token = await resolveClientToken({ home, tokenFile });
  let shutdownAccepted = false;
  try {
    const response = await fetch(
      new URL("/control/shutdown", record.serverUrl),
      {
        method: "POST",
        headers: token ? { Authorization: `Bearer ${token}` } : {},
        signal: AbortSignal.timeout(
          Math.min(SHUTDOWN_REQUEST_TIMEOUT_MS, Math.max(timeoutMs, 100)),
        ),
      },
    );
    if (!response.ok) {
      throw new ShutdownResponseError(response.status);
    }
    shutdownAccepted = true;
  } catch (error) {
    if (error instanceof ShutdownResponseError) throw error;
  }

  if (
    shutdownAccepted &&
    (await waitForProcessExit(record.pid, Math.max(timeoutMs, 0)))
  ) {
    await removeStoppedInstanceRecord(home, record);
    return { running: false, ready: false };
  }
  return forceStopRecordedProcess(home, record, shutdownAccepted, timeoutMs);
}

class ShutdownResponseError extends Error {
  constructor(status: number) {
    super(`Server shutdown request failed with HTTP ${status}`);
  }
}

async function forceStopRecordedProcess(
  home: string | undefined,
  record: DaemonInstanceRecord,
  allowMissingRecord: boolean,
  timeoutMs: number,
): Promise<DaemonControlStatus> {
  if (!processIsAlive(record.pid)) {
    await removeStoppedInstanceRecord(home, record);
    return { running: false, ready: false };
  }
  await assertSameInstanceBeforeSignal(home, record, allowMissingRecord);
  signalProcess(record.pid, "SIGTERM");
  const graceMs = Math.min(TERMINATION_GRACE_MS, Math.max(timeoutMs, 100));
  if (await waitForProcessExit(record.pid, graceMs)) {
    await removeStoppedInstanceRecord(home, record);
    return { running: false, ready: false };
  }

  await assertSameInstanceBeforeSignal(home, record, true);
  signalProcess(record.pid, "SIGKILL");
  if (!(await waitForProcessExit(record.pid, graceMs))) {
    throw new Error(
      `Timed out waiting for zvec-grep server process ${record.pid} to stop after SIGKILL.`,
    );
  }
  await removeStoppedInstanceRecord(home, record);
  return { running: false, ready: false };
}

async function assertSameInstanceBeforeSignal(
  home: string | undefined,
  expected: DaemonInstanceRecord,
  allowMissing: boolean,
): Promise<void> {
  const current = await readInstanceRecord(home);
  if (!current && allowMissing) return;
  if (
    !current ||
    current.hostname !== expected.hostname ||
    current.pid !== expected.pid ||
    current.instanceToken !== expected.instanceToken
  ) {
    throw new Error(
      "zvec-grep server instance changed while stopping; refusing to signal the recorded process.",
    );
  }
}

function signalProcess(pid: number, signal: NodeJS.Signals): void {
  try {
    process.kill(pid, signal);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ESRCH") throw error;
  }
}

async function removeStoppedInstanceRecord(
  home: string | undefined,
  stopped: DaemonInstanceRecord,
): Promise<void> {
  const path = join(daemonHome(home), "instance.lock");
  const current = await readRecordPath(path);
  if (
    current?.pid === stopped.pid &&
    current.instanceToken === stopped.instanceToken
  ) {
    await unlink(path).catch(() => undefined);
  }
}

async function assertListenAddressAvailable(
  host: string,
  port: number,
): Promise<void> {
  const probe = createServer();
  try {
    await new Promise<void>((resolve, reject) => {
      probe.once("error", reject);
      probe.listen({ host, port, exclusive: true }, () => {
        probe.removeListener("error", reject);
        probe.close((error) => (error ? reject(error) : resolve()));
      });
    });
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "EADDRINUSE") {
      throw new Error(
        `Server address ${host}:${port} is already in use. Another or legacy zvec-grep server may still be running.`,
        { cause: error },
      );
    }
    throw error;
  }
}

async function waitForProcessExit(
  pid: number,
  timeoutMs: number,
): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!processIsAlive(pid)) return true;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  return !processIsAlive(pid);
}

async function waitForStatus(
  home: string | undefined,
  running: boolean,
  timeoutMs: number,
): Promise<DaemonControlStatus> {
  const deadline = Date.now() + timeoutMs;
  let last: DaemonControlStatus = { running: false, ready: false };
  while (Date.now() < deadline) {
    last = await serverStatus(home);
    if (running ? last.running && last.ready : !last.running) return last;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(
    `Timed out waiting for zvec-grep server to ${running ? "start" : "stop"}.`,
  );
}

async function waitForRunningRecord(
  home: string | undefined,
  timeoutMs: number,
): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if ((await serverStatus(home)).running) return true;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  return false;
}

async function readRecordPath(
  path: string,
): Promise<DaemonInstanceRecord | undefined> {
  try {
    const value = JSON.parse(
      await readFile(path, "utf8"),
    ) as Partial<DaemonInstanceRecord>;
    if (
      typeof value.pid !== "number" ||
      typeof value.hostname !== "string" ||
      typeof value.instanceToken !== "string" ||
      typeof value.startedAt !== "number" ||
      typeof value.updatedAt !== "number" ||
      typeof value.serverUrl !== "string" ||
      typeof value.ready !== "boolean" ||
      (value.mcpToolset !== undefined &&
        value.mcpToolset !== "agent" &&
        value.mcpToolset !== "full")
    )
      return undefined;
    return {
      ...(value as Omit<DaemonInstanceRecord, "mcpToolset">),
      // Locks written before toolset profiles exposed the full six-tool MCP
      // surface, so preserve that behavior when reporting a live legacy daemon.
      mcpToolset: value.mcpToolset ?? "full",
    };
  } catch {
    return undefined;
  }
}
