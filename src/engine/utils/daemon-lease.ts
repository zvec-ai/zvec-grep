import { hostname } from "node:os";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { EngineError } from "../errors/index.js";
import { workspaceHome } from "../service/root.js";

export type DaemonLeaseRecord = {
  pid: number;
  hostname: string;
  instanceToken: string;
  createdAt: number;
  updatedAt: number;
};

export function daemonLeasePath(root: string): string {
  return join(workspaceHome(root), "locks", "daemon.json");
}

export function daemonLeaseGuardPath(root: string): string {
  return join(workspaceHome(root), "locks", "daemon.guard");
}

export type DaemonLeaseGuard = {
  release(): void;
};

export function acquireDaemonLeaseGuard(
  root: string,
  instanceToken: string,
): DaemonLeaseGuard | undefined {
  const path = daemonLeaseGuardPath(root);
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  const record: DaemonLeaseRecord = {
    pid: process.pid,
    hostname: hostname(),
    instanceToken,
    createdAt: Date.now(),
    updatedAt: Date.now(),
  };
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      mkdirSync(path, { recursive: false, mode: 0o700 });
      writeFileSync(join(path, "owner.json"), `${JSON.stringify(record)}\n`, {
        mode: 0o600,
      });
      let released = false;
      return {
        release() {
          if (!released) {
            released = true;
            rmSync(path, { recursive: true, force: true });
          }
        },
      };
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") {
        throw error;
      }
      if (!guardIsStale(path)) {
        return undefined;
      }
      const quarantine = `${path}.stale-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
      try {
        renameSync(path, quarantine);
      } catch (renameError) {
        if ((renameError as NodeJS.ErrnoException).code === "ENOENT") {
          continue;
        }
        return undefined;
      }
      rmSync(quarantine, { recursive: true, force: true });
    }
  }
  return undefined;
}

export function readDaemonLease(root: string): DaemonLeaseRecord | undefined {
  const path = daemonLeasePath(root);
  if (!existsSync(path)) {
    return undefined;
  }
  try {
    const parsed = JSON.parse(
      readFileSync(path, "utf8"),
    ) as Partial<DaemonLeaseRecord>;
    if (
      typeof parsed.pid !== "number" ||
      typeof parsed.hostname !== "string" ||
      typeof parsed.instanceToken !== "string" ||
      typeof parsed.createdAt !== "number" ||
      typeof parsed.updatedAt !== "number"
    ) {
      return undefined;
    }
    return parsed as DaemonLeaseRecord;
  } catch {
    return undefined;
  }
}

export function daemonLeaseFileIsAbandoned(root: string): boolean {
  try {
    return Date.now() - statSync(daemonLeasePath(root)).mtimeMs > 30_000;
  } catch {
    return true;
  }
}

export function assertDaemonWriteAllowed(
  root: string,
  instanceToken?: string,
): DaemonLeaseGuard | undefined {
  const path = daemonLeasePath(root);
  const initial = readDaemonLease(root);
  if (initial?.pid === process.pid && initial.instanceToken === instanceToken) {
    return undefined;
  }
  const guard = acquireDaemonLeaseGuard(
    root,
    instanceToken ?? `direct-${process.pid}`,
  );
  if (!guard) {
    throw daemonLeaseActiveError(root, initial?.pid ?? 0);
  }
  const record = readDaemonLease(root);
  if (!existsSync(path)) {
    return guard;
  }
  if (!record) {
    if (daemonLeaseFileIsAbandoned(root)) {
      unlinkSync(path);
      return guard;
    }
    guard.release();
    throw new EngineError("A zvec-grep daemon lease exists for this root", {
      code: "ZVEC_GREP.ENGINE.DAEMON_LEASE_ACTIVE",
      context: `root=${root}`,
    });
  }
  if (record.pid === process.pid && record.instanceToken === instanceToken) {
    guard.release();
    return undefined;
  }
  if (record.hostname === hostname() && !processIsAlive(record.pid)) {
    unlinkSync(path);
    return guard;
  }
  guard.release();
  throw daemonLeaseActiveError(root, record.pid);
}

function daemonLeaseActiveError(root: string, pid: number): EngineError {
  return new EngineError("A zvec-grep daemon owns index writes for this root", {
    code: "ZVEC_GREP.ENGINE.DAEMON_LEASE_ACTIVE",
    context: [
      `root=${root}`,
      `pid=${pid}`,
      "hint=Run with --mode auto so a ready daemon handles indexed operations. If auto is already active, check zg server status and restore or stop the daemon before retrying.",
      'config=Edit ~/.zvec-grep/config.json and set client.mode to "auto" to persist this behavior.',
    ].join("\n"),
  });
}

function guardIsStale(path: string): boolean {
  try {
    const record = JSON.parse(
      readFileSync(join(path, "owner.json"), "utf8"),
    ) as Partial<DaemonLeaseRecord>;
    if (
      record.hostname === hostname() &&
      typeof record.pid === "number" &&
      typeof record.instanceToken === "string" &&
      typeof record.createdAt === "number" &&
      typeof record.updatedAt === "number"
    ) {
      return !processIsAlive(record.pid);
    }
    if (
      typeof record.hostname === "string" &&
      typeof record.pid === "number" &&
      typeof record.instanceToken === "string" &&
      typeof record.createdAt === "number" &&
      typeof record.updatedAt === "number"
    ) {
      return Date.now() - record.updatedAt > 30_000;
    }
    return Date.now() - statSync(path).mtimeMs > 30_000;
  } catch {
    try {
      return Date.now() - statSync(path).mtimeMs > 30_000;
    } catch {
      return true;
    }
  }
}

export function processIsAlive(pid: number): boolean {
  if (!Number.isInteger(pid) || pid <= 0) {
    return false;
  }
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}
