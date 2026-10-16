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
import { detail, EngineError, errorDetails } from "../errors.js";
import { INCOMPLETE_RECOVERY_HINT } from "./recovery-guidance.js";

export type FileLock = {
  readonly path: string;
  readonly info: FileLockInfo;
  /**
   * True when this handle actually released its lock; false when the lock
   * was already gone or ownership evidence no longer matches (in which case
   * nothing was deleted). Callers that treat release as a commit point must
   * check this result.
   */
  release(): boolean;
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
      const identity = lockDirectoryIdentity(lockPath);
      const info = currentLockInfo(options.operation);
      writeFileSync(
        lockInfoPath(lockPath),
        `${JSON.stringify(info, null, 2)}\n`,
        "utf8",
      );

      return {
        path: lockPath,
        info,
        release: () => releaseFileLock(lockPath, info, identity),
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
    // The acquisition identity is retained for every release path: never
    // recapture identity at cleanup time.
    let identity: LockDirectoryIdentity | undefined;
    try {
      mkdirSync(readerPath, { recursive: true });
      identity = lockDirectoryIdentity(readerPath);
      writeFileSync(
        lockInfoPath(readerPath),
        `${JSON.stringify(info, null, 2)}\n`,
        "utf8",
      );

      if (existsSync(writePath)) {
        releaseFileLock(readerPath, info, identity);
        if (cleanupStaleLock(writePath, staleMs)) {
          continue;
        }

        throw lockBusyError(writePath, options.operation);
      }

      return {
        path: readerPath,
        info,
        release: () => releaseFileLock(readerPath, info, identity),
      };
    } catch (error) {
      releaseFileLock(readerPath, info, identity);
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

function releaseFileLock(
  lockPath: string,
  owner: FileLockInfo,
  expectedIdentity: LockDirectoryIdentity | undefined,
): boolean {
  const current = readLockInfo(lockPath);
  if (current?.token !== owner.token) {
    return false;
  }
  // Physical ownership: token equality alone cannot distinguish a replaced
  // directory carrying a copied token. Never delete another directory's
  // lock through this handle.
  if (!lockIdentityMatches(lockPath, expectedIdentity)) {
    return false;
  }

  rmSync(lockPath, { recursive: true, force: true });
  return true;
}

type LockDirectoryIdentity = {
  device: number;
  inode: number;
};

function lockDirectoryIdentity(
  lockPath: string,
): LockDirectoryIdentity | undefined {
  try {
    const info = statSync(lockPath);
    return { device: info.dev, inode: info.ino };
  } catch {
    return undefined;
  }
}

function lockIdentityMatches(
  lockPath: string,
  expected: LockDirectoryIdentity | undefined,
): boolean {
  if (!expected) {
    // No reference identity was captured; only a vanished directory counts
    // as a match (nothing remains to delete).
    return lockDirectoryIdentity(lockPath) === undefined;
  }
  const current = lockDirectoryIdentity(lockPath);
  return (
    current !== undefined &&
    current.device === expected.device &&
    current.inode === expected.inode
  );
}

function cleanupStaleLock(
  lockPath: string,
  staleMs: number,
  options: { allowReclaim: boolean } = { allowReclaim: false },
): boolean {
  // Write locks are never reclaimed automatically: removal of another
  // process's lock cannot be made race-free, so any held or uncertain write
  // lock blocks until an operator recovers it after writers are quiescent.
  // Only uniquely-named reader entries may be reclaimed, and only for a
  // verified-dead local owner.
  if (!options.allowReclaim) {
    return false;
  }
  if (!existsSync(lockPath)) {
    return false;
  }

  const identityBefore = lockDirectoryIdentity(lockPath);
  const info = readLockInfo(lockPath);
  if (!isStaleLock(lockPath, info, staleMs)) {
    return false;
  }
  // Re-verify the observed token and physical identity before deleting
  // another owner's lock: a changed or replaced directory is never reclaimed
  // through this path.
  const infoNow = readLockInfo(lockPath);
  if (infoNow?.token !== info?.token) {
    return false;
  }
  if (!lockIdentityMatches(lockPath, identityBefore)) {
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
  } catch (error) {
    if (isNodeError(error) && error.code === "ENOENT") {
      return false;
    }
    // Enumeration/access failures are uncertainty, not an empty directory:
    // block the writer rather than admit it beside unreadable readers.
    return true;
  }

  let active = false;
  for (const entry of entries) {
    const readerPath = join(readersPath, entry);
    if (cleanupStaleLock(readerPath, staleMs, { allowReclaim: true })) {
      continue;
    }

    active = true;
  }

  return active;
}

type LockLiveness = "alive" | "dead" | "unknown";

function processLiveness(pid: number): LockLiveness {
  if (!Number.isInteger(pid) || pid <= 0) {
    return "unknown";
  }

  try {
    process.kill(pid, 0);
    return "alive";
  } catch (error) {
    if (isNodeError(error)) {
      if (error.code === "ESRCH") {
        return "dead";
      }
      if (error.code === "EPERM") {
        return "alive";
      }
    }
    // Unexpected probe failures are unknown, never evidence of death.
    return "unknown";
  }
}

function isStaleLock(
  lockPath: string,
  info: FileLockInfo | null,
  staleMs: number,
): boolean {
  void lockPath;
  void staleMs;
  // Age never proves inactivity: a known-live local owner is never stale at
  // any age, and unknown ownership (foreign host, missing or corrupt
  // metadata, invalid fields, unexpected probe failures) must remain blocked
  // rather than be reclaimed. Reclamation is safe only for a verified-dead
  // local owner. Recovering any other lock is an explicit operator action
  // after writers are quiescent.
  if (!info) {
    return false;
  }
  if (info.hostname !== hostname()) {
    return false;
  }
  return processLiveness(info.pid) === "dead";
}

function readLockInfo(lockPath: string): FileLockInfo | null {
  try {
    const parsed = JSON.parse(
      readFileSync(lockInfoPath(lockPath), "utf8"),
    ) as Partial<FileLockInfo>;
    // Semantic validation: invalid ownership metadata is unknown, never
    // evidence about the owner's state.
    if (
      typeof parsed.token === "string" &&
      parsed.token.length > 0 &&
      Number.isInteger(parsed.pid) &&
      (parsed.pid as number) > 0 &&
      typeof parsed.hostname === "string" &&
      parsed.hostname.length > 0 &&
      Number.isFinite(parsed.startedAt) &&
      typeof parsed.operation === "string"
    ) {
      return parsed as FileLockInfo;
    }
  } catch {
    return null;
  }

  return null;
}

function lockBusyError(
  lockPath: string,
  requestedOperation: string,
): EngineError {
  const owner = readLockInfo(lockPath);
  const state = ownerState(owner);

  return new EngineError("Index unavailable", {
    code: "ZVEC_GREP.ENGINE.LOCK.BUSY",
    context: errorDetails([
      detail("lock", lockPath),
      detail("operation", requestedOperation),
      detail("ownerOperation", owner?.operation),
      detail("ownerPid", owner?.pid),
      detail("ownerHost", owner?.hostname),
      detail("ownerState", state),
      detail("hint", lockRecoveryHint(state)),
    ]),
  });
}

function readLockBusyError(
  lockPath: string,
  requestedOperation: string,
): EngineError {
  const owner = firstActiveReaderInfo(lockPath);
  const state = ownerState(owner);

  return new EngineError("Index unavailable", {
    code: "ZVEC_GREP.ENGINE.LOCK.BUSY",
    context: errorDetails([
      detail("lock", readersLockPath(lockPath)),
      detail("operation", requestedOperation),
      detail("ownerOperation", owner?.operation),
      detail("ownerPid", owner?.pid),
      detail("ownerHost", owner?.hostname),
      detail("ownerState", state),
      detail("hint", lockRecoveryHint(state)),
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

function ownerState(owner: FileLockInfo | null): LockLiveness {
  return owner?.hostname === hostname()
    ? processLiveness(owner.pid)
    : "unknown";
}

function lockRecoveryHint(state: LockLiveness): string {
  if (state === "alive")
    return "Another operation is active. Wait for it to finish or cancel it through its owner. Do not remove an active lock.";
  if (state === "unknown")
    return "Owner activity cannot be confirmed. Write locks are never reclaimed automatically. Verify the recorded owner on its host before operator recovery. Do not remove an unconfirmed lock. If the destination is INCOMPLETE, preserve its entire index home for inspection; see docs/09-portable-indexes.md.";
  return `The recorded local owner has exited. Write locks remain as safety barriers; dead local reader entries are checked separately. ${INCOMPLETE_RECOVERY_HINT}`;
}
