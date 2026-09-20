import { randomUUID } from "node:crypto";
import {
  lstat,
  mkdir,
  readFile,
  readdir,
  rename,
  rm,
  rmdir,
  stat,
  unlink,
  utimes,
  writeFile,
} from "node:fs/promises";
import { hostname } from "node:os";
import { join } from "node:path";

export type ModelArtifactCacheLock = {
  touch(): Promise<void>;
  assertOwned(): Promise<void>;
  release(): Promise<void>;
};

type LockOptions = {
  pollMs: number;
  staleMs: number;
  heartbeatMs: number;
  dependencies: {
    now: () => number;
    setTimeout: (
      callback: () => void,
      ms: number,
    ) => ReturnType<typeof setTimeout>;
  };
};

type LockObservation = {
  device: number;
  inode: number;
  newestHeartbeat: number;
  deadOwner: boolean;
};

export async function acquireModelArtifactCacheLock(
  lockPath: string,
  options: LockOptions,
): Promise<ModelArtifactCacheLock> {
  while (true) {
    const lock = await tryAcquire(lockPath, options);
    if (lock) {
      return lock;
    }
    if (await removeAbandonedLock(lockPath, options)) {
      continue;
    }
    await new Promise<void>((resolve) => {
      options.dependencies.setTimeout(resolve, options.pollMs);
    });
  }
}

async function tryAcquire(
  lockPath: string,
  options: LockOptions,
): Promise<ModelArtifactCacheLock | undefined> {
  try {
    // Preserve the expiry policy for existing locks, including old empty ones.
    await lstat(lockPath);
    return undefined;
  } catch (error) {
    if (!isMissingFileError(error)) {
      throw error;
    }
  }

  const token = randomUUID();
  const ownerName = `.owner-${token}`;
  const ownerPath = join(lockPath, ownerName);
  const stagingPath = `${lockPath}.pending-${process.pid}-${token}`;
  await mkdir(stagingPath);
  try {
    await writeFile(
      join(stagingPath, ownerName),
      `${JSON.stringify({ token, pid: process.pid, hostname: hostname(), createdAt: options.dependencies.now() })}\n`,
      { flag: "wx" },
    );
    // Publish an already initialized, nonempty directory. Concurrent publishers
    // cannot replace one another's locks or write owners into a successor's lock.
    try {
      await rename(stagingPath, lockPath);
    } catch (error) {
      if (await isDirectoryConflict(error, lockPath)) {
        return undefined;
      }
      throw error;
    }
  } finally {
    await rm(stagingPath, { recursive: true, force: true });
  }

  let lastHeartbeat = options.dependencies.now();
  let released = false;
  const refresh = async (force: boolean): Promise<void> => {
    const now = options.dependencies.now();
    if (!force && now - lastHeartbeat < options.heartbeatMs) {
      return;
    }
    // A replaced directory cannot contain this owner's unique token. Updating
    // the token file therefore also fences an owner after a stale takeover.
    await utimes(ownerPath, new Date(now), new Date(now));
    lastHeartbeat = now;
  };
  const lock: ModelArtifactCacheLock = {
    async touch() {
      await refresh(false);
    },
    async assertOwned() {
      await refresh(true);
    },
    async release() {
      if (released) {
        return;
      }
      released = true;
      try {
        await unlink(ownerPath);
      } catch (error) {
        if (isMissingFileError(error)) {
          return;
        }
        throw error;
      }
      try {
        await rmdir(lockPath);
      } catch (error) {
        if (
          !isMissingFileError(error) &&
          !isErrorCode(error, "ENOTEMPTY") &&
          !isErrorCode(error, "EEXIST")
        ) {
          throw error;
        }
      }
    },
  };
  try {
    // Initialization may have been paused for longer than the lease. Refresh on
    // publication and verify that this owner has not already been displaced.
    await lock.assertOwned();
    return lock;
  } catch (error) {
    await lock.release().catch(() => undefined);
    if (isMissingFileError(error)) {
      return undefined;
    }
    throw error;
  }
}

async function isDirectoryConflict(
  error: unknown,
  path: string,
): Promise<boolean> {
  if (isErrorCode(error, "EEXIST") || isErrorCode(error, "ENOTEMPTY")) {
    return true;
  }
  // Windows reports EPERM when rename targets an existing directory. Do not
  // mistake a permission failure with no competing directory for contention.
  if (isErrorCode(error, "EPERM")) {
    return await lstat(path).then(
      (stats) => stats.isDirectory(),
      () => false,
    );
  }
  return false;
}

async function removeAbandonedLock(
  lockPath: string,
  options: LockOptions,
): Promise<boolean> {
  try {
    const observed = await inspectLock(lockPath);
    if (!isAbandoned(observed, options)) {
      return false;
    }
    const stalePath = `${lockPath}.stale-${process.pid}-${randomUUID()}`;
    await rename(lockPath, stalePath);
    const moved = await inspectLock(stalePath);
    if (
      moved.device !== observed.device ||
      moved.inode !== observed.inode ||
      !isAbandoned(moved, options)
    ) {
      // Another owner may have acquired or refreshed the lock while we were
      // inspecting it. Restore that directory, never delete its owner file.
      try {
        await rename(stalePath, lockPath);
      } catch (error) {
        if (!(await isDirectoryConflict(error, lockPath))) {
          throw error;
        }
        // A successor already owns lockPath. Keep its directory untouched;
        // the displaced owner is fenced by its missing token at lockPath.
      }
      return false;
    }
    await rm(stalePath, { recursive: true, force: true });
    return true;
  } catch (error) {
    if (isMissingFileError(error)) {
      return true;
    }
    throw error;
  }
}

function isAbandoned(
  observation: LockObservation,
  options: LockOptions,
): boolean {
  return (
    observation.deadOwner ||
    options.dependencies.now() - observation.newestHeartbeat >= options.staleMs
  );
}

async function inspectLock(lockPath: string): Promise<LockObservation> {
  const lockStats = await stat(lockPath);
  let newestHeartbeat = lockStats.mtimeMs;
  const owners = (await readdir(lockPath, { withFileTypes: true })).filter(
    (entry) => entry.isFile() && entry.name.startsWith(".owner-"),
  );
  for (const owner of owners) {
    try {
      const ownerStats = await stat(join(lockPath, owner.name));
      newestHeartbeat = Math.max(newestHeartbeat, ownerStats.mtimeMs);
    } catch (error) {
      if (!isMissingFileError(error)) {
        throw error;
      }
    }
  }
  return {
    device: lockStats.dev,
    inode: lockStats.ino,
    newestHeartbeat,
    deadOwner:
      owners.length === 1 && (await isKnownDeadOwner(lockPath, owners[0].name)),
  };
}

async function isKnownDeadOwner(
  lockPath: string,
  ownerName: string,
): Promise<boolean> {
  let value: unknown;
  try {
    value = JSON.parse(await readFile(join(lockPath, ownerName), "utf8"));
  } catch (error) {
    if (isMissingFileError(error) || error instanceof SyntaxError) {
      return false;
    }
    throw error;
  }
  if (value === null || typeof value !== "object") {
    return false;
  }
  const owner = value as Record<string, unknown>;
  if (
    owner.hostname !== hostname() ||
    typeof owner.token !== "string" ||
    owner.token.length === 0 ||
    `.owner-${owner.token}` !== ownerName ||
    typeof owner.pid !== "number" ||
    !Number.isSafeInteger(owner.pid) ||
    owner.pid <= 0 ||
    owner.pid > 2_147_483_647
  ) {
    return false;
  }
  try {
    process.kill(owner.pid, 0);
    return false;
  } catch (error) {
    // Only ESRCH proves this local PID has exited. EPERM and all other errors
    // are inconclusive and retain the ordinary heartbeat expiry period.
    return isErrorCode(error, "ESRCH");
  }
}

function isMissingFileError(error: unknown): boolean {
  return isErrorCode(error, "ENOENT") || isErrorCode(error, "ENOTDIR");
}

function isErrorCode(error: unknown, code: string): boolean {
  return (
    error !== null &&
    typeof error === "object" &&
    "code" in error &&
    error.code === code
  );
}
