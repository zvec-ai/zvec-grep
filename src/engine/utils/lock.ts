import { randomUUID } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { hostname } from "node:os";
import { dirname, join } from "node:path";
import { detail, EngineError, errorDetails } from "../errors/index.js";

export type FileLock = {
  readonly path: string;
  readonly info: FileLockInfo;
  release(): void;
};

export type FileLockInfo = {
  token: string;
  pid: number;
  hostname: string;
  startedAt: number;
  operation: string;
};

type FileLockOptions = {
  operation: string;
  staleMs?: number;
};

const LOCK_INFO_FILE = "lock.json";
const DEFAULT_STALE_LOCK_MS = 6 * 60 * 60 * 1000;

function acquireExclusiveDirectoryLock(
  lockPath: string,
  options: FileLockOptions,
): FileLock {
  mkdirSync(dirname(lockPath), { recursive: true });

  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      mkdirSync(lockPath);
      const info = currentLockInfo(options.operation);
      writeFileSync(
        lockInfoPath(lockPath),
        `${JSON.stringify(info, null, 2)}\n`,
        "utf8",
      );

      return {
        path: lockPath,
        info,
        release: () => releaseFileLock(lockPath, info),
      };
    } catch (error) {
      if (!isNodeError(error) || error.code !== "EEXIST") {
        throw error;
      }

      if (
        cleanupStaleLock(lockPath, options.staleMs ?? DEFAULT_STALE_LOCK_MS)
      ) {
        continue;
      }

      throw lockBusyError(lockPath, options.operation);
    }
  }

  throw lockBusyError(lockPath, options.operation);
}

function assertExclusiveDirectoryUnlocked(
  lockPath: string,
  operation: string,
): void {
  if (!existsSync(lockPath)) {
    return;
  }

  if (cleanupStaleLock(lockPath, DEFAULT_STALE_LOCK_MS)) {
    return;
  }

  throw lockBusyError(lockPath, operation);
}

export function acquireReadWriteLock(
  lockPath: string,
  mode: "read" | "write",
  options: FileLockOptions,
): FileLock {
  return mode === "read"
    ? acquireReadLock(lockPath, options)
    : acquireWriteLock(lockPath, options);
}

export function assertNoWriteLock(lockPath: string, operation: string): void {
  assertExclusiveDirectoryUnlocked(writeLockPath(lockPath), operation);
}

function currentLockInfo(operation: string): FileLockInfo {
  return {
    token: randomUUID(),
    pid: process.pid,
    hostname: hostname(),
    startedAt: Date.now(),
    operation,
  };
}

function acquireReadLock(lockPath: string, options: FileLockOptions): FileLock {
  mkdirSync(dirname(lockPath), { recursive: true });
  const staleMs = options.staleMs ?? DEFAULT_STALE_LOCK_MS;
  const writePath = writeLockPath(lockPath);

  for (let attempt = 0; attempt < 2; attempt++) {
    if (existsSync(writePath)) {
      if (cleanupStaleLock(writePath, staleMs)) {
        continue;
      }

      throw lockBusyError(writePath, options.operation);
    }

    const info = currentLockInfo(options.operation);
    const readerPath = join(
      readersLockPath(lockPath),
      `${info.pid}-${info.token}`,
    );
    try {
      mkdirSync(readerPath, { recursive: true });
      writeFileSync(
        lockInfoPath(readerPath),
        `${JSON.stringify(info, null, 2)}\n`,
        "utf8",
      );

      if (existsSync(writePath)) {
        releaseFileLock(readerPath, info);
        if (cleanupStaleLock(writePath, staleMs)) {
          continue;
        }

        throw lockBusyError(writePath, options.operation);
      }

      return {
        path: readerPath,
        info,
        release: () => releaseFileLock(readerPath, info),
      };
    } catch (error) {
      releaseFileLock(readerPath, info);
      if (!isNodeError(error) || error.code !== "EEXIST") {
        throw error;
      }
    }
  }

  throw lockBusyError(writePath, options.operation);
}

function acquireWriteLock(
  lockPath: string,
  options: FileLockOptions,
): FileLock {
  const staleMs = options.staleMs ?? DEFAULT_STALE_LOCK_MS;
  const writePath = writeLockPath(lockPath);

  for (let attempt = 0; attempt < 2; attempt++) {
    const lock = acquireExclusiveDirectoryLock(writePath, options);
    if (!hasActiveReaders(lockPath, staleMs)) {
      return lock;
    }

    lock.release();
    throw readLockBusyError(lockPath, options.operation);
  }

  throw readLockBusyError(lockPath, options.operation);
}

function releaseFileLock(lockPath: string, owner: FileLockInfo): void {
  const current = readLockInfo(lockPath);
  if (current?.token !== owner.token) {
    return;
  }

  rmSync(lockPath, { recursive: true, force: true });
}

function cleanupStaleLock(lockPath: string, staleMs: number): boolean {
  if (!existsSync(lockPath)) {
    return false;
  }

  const info = readLockInfo(lockPath);
  if (!isStaleLock(lockPath, info, staleMs)) {
    return false;
  }

  rmSync(lockPath, { recursive: true, force: true });
  return true;
}

function hasActiveReaders(lockPath: string, staleMs: number): boolean {
  const readersPath = readersLockPath(lockPath);
  if (!existsSync(readersPath)) {
    return false;
  }

  let entries: string[];
  try {
    entries = readdirSync(readersPath);
  } catch {
    return false;
  }

  let active = false;
  for (const entry of entries) {
    const readerPath = join(readersPath, entry);
    if (cleanupStaleLock(readerPath, staleMs)) {
      continue;
    }

    active = true;
  }

  return active;
}

function isStaleLock(
  lockPath: string,
  info: FileLockInfo | null,
  staleMs: number,
): boolean {
  const now = Date.now();
  const startedAt = info?.startedAt ?? lockDirectoryMtime(lockPath);
  const expired = now - startedAt > staleMs;
  if (!info) {
    return expired;
  }

  if (info.hostname === hostname() && !processIsAlive(info.pid)) {
    return true;
  }

  return expired;
}

function processIsAlive(pid: number): boolean {
  if (!Number.isInteger(pid) || pid <= 0) {
    return false;
  }

  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return isNodeError(error) && error.code === "EPERM";
  }
}

function readLockInfo(lockPath: string): FileLockInfo | null {
  try {
    const parsed = JSON.parse(
      readFileSync(lockInfoPath(lockPath), "utf8"),
    ) as Partial<FileLockInfo>;
    if (
      typeof parsed.token === "string" &&
      typeof parsed.pid === "number" &&
      typeof parsed.hostname === "string" &&
      typeof parsed.startedAt === "number" &&
      typeof parsed.operation === "string"
    ) {
      return parsed as FileLockInfo;
    }
  } catch {
    return null;
  }

  return null;
}

function lockDirectoryMtime(lockPath: string): number {
  try {
    return statSync(lockPath).mtimeMs;
  } catch {
    return Date.now();
  }
}

function lockBusyError(
  lockPath: string,
  requestedOperation: string,
): EngineError {
  const owner = readLockInfo(lockPath);

  return new EngineError("Index unavailable", {
    code: "ZVEC_GREP.ENGINE.LOCK.BUSY",
    context: errorDetails([
      detail("lock", lockPath),
      detail("operation", requestedOperation),
      detail("ownerOperation", owner?.operation),
      detail("ownerPid", owner?.pid),
      detail("ownerHost", owner?.hostname),
    ]),
  });
}

function readLockBusyError(
  lockPath: string,
  requestedOperation: string,
): EngineError {
  const owner = firstActiveReaderInfo(lockPath);

  return new EngineError("Index unavailable", {
    code: "ZVEC_GREP.ENGINE.LOCK.BUSY",
    context: errorDetails([
      detail("lock", readersLockPath(lockPath)),
      detail("operation", requestedOperation),
      detail("ownerOperation", owner?.operation),
      detail("ownerPid", owner?.pid),
      detail("ownerHost", owner?.hostname),
    ]),
  });
}

function firstActiveReaderInfo(lockPath: string): FileLockInfo | null {
  const readersPath = readersLockPath(lockPath);
  if (!existsSync(readersPath)) {
    return null;
  }

  try {
    for (const entry of readdirSync(readersPath)) {
      const info = readLockInfo(join(readersPath, entry));
      if (info) {
        return info;
      }
    }
  } catch {
    return null;
  }

  return null;
}

function lockInfoPath(lockPath: string): string {
  return join(lockPath, LOCK_INFO_FILE);
}

function writeLockPath(lockPath: string): string {
  return `${lockPath}.write`;
}

function readersLockPath(lockPath: string): string {
  return `${lockPath}.readers`;
}

function isNodeError(error: unknown): error is NodeJS.ErrnoException {
  return typeof error === "object" && error !== null && "code" in error;
}
