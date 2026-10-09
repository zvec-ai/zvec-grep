import { readdirSync, realpathSync, statSync, unlinkSync } from "node:fs";
import { join } from "node:path";
import { readJsonFileSync, writeJsonFileSync } from "./utils/json.js";
import { acquireReadWriteLock } from "./utils/lock.js";
import { defaultHome } from "./utils/path.js";

/**
 * Host-local workspace binding records: which physical workspace and storage
 * instances have been content-verified for a given portable index. This
 * store lives in the global home, never inside the workspace, so transferred
 * indexes carry no verification claim. Whenever the current binding cannot
 * be established against the record, the index is treated as unverified and
 * its next indexing run reconciles by content hash.
 */

export type WorkspaceBinding = {
  /** Actual filesystem spelling (realpath) of the verified workspace root. */
  rootPath: string;
  rootDevice: number;
  rootInode: number;
  /** Storage instance identity (`<root>/.zvec-grep` at verification time). */
  homeDevice: number;
  homeInode: number;
  verifiedTime: number;
};

type BindingRecord = {
  version: 1;
  bindings: WorkspaceBinding[];
};

const BINDINGS_DIRECTORY = "bindings";
const MAX_BINDINGS_PER_INDEX = 8;
const MAX_INDEX_ENTRIES = 1024;

export function currentWorkspaceBinding(
  workspaceRoot: string,
): Omit<WorkspaceBinding, "verifiedTime"> | null {
  // Resolve the actual filesystem spelling before recording identity: NFC
  // canonical document identities must not change which physical root gets
  // inspected (distinct NFC/NFD root directories are different bindings).
  try {
    // Match fs.promises.realpath in the daemon. On Windows the JavaScript
    // resolver can preserve 8.3 path aliases that native realpath expands.
    const rootPath = realpathSync.native(workspaceRoot);
    const rootInfo = statSync(rootPath);
    const homeInfo = statSync(join(rootPath, ".zvec-grep"));
    return {
      rootPath,
      rootDevice: rootInfo.dev,
      rootInode: rootInfo.ino,
      homeDevice: homeInfo.dev,
      homeInode: homeInfo.ino,
    };
  } catch {
    return null;
  }
}

/**
 * Resolve the workspace root for invalidation. Genuine absence returns
 * undefined; permission and I/O failures propagate — invalidation must never
 * report success when identity could not be resolved.
 */
function resolveBindingRoot(workspaceRoot: string): string | undefined {
  try {
    return realpathSync.native(workspaceRoot);
  } catch (error) {
    if (
      isNodeError(error) &&
      (error.code === "ENOENT" || error.code === "ENOTDIR")
    ) {
      return undefined;
    }
    throw error;
  }
}

function isNodeError(error: unknown): error is NodeJS.ErrnoException {
  return typeof error === "object" && error !== null && "code" in error;
}

function sameBinding(
  binding: WorkspaceBinding,
  current: Omit<WorkspaceBinding, "verifiedTime">,
): boolean {
  return (
    binding.rootPath === current.rootPath &&
    binding.rootDevice === current.rootDevice &&
    binding.rootInode === current.rootInode &&
    binding.homeDevice === current.homeDevice &&
    binding.homeInode === current.homeInode
  );
}

export class WorkspaceBindingStore {
  constructor(private readonly home: string = defaultHome()) {}

  /**
   * True when a record proves this index was content-verified at the current
   * binding. Any doubt — missing store, missing entry, changed path or
   * changed filesystem identity — means unverified.
   */
  matches(indexId: string, workspaceRoot: string): boolean {
    const current = currentWorkspaceBinding(workspaceRoot);
    if (!current) {
      return false;
    }
    return this.readBindings(indexId).some(
      (binding) =>
        binding.rootPath === current.rootPath &&
        binding.rootDevice === current.rootDevice &&
        binding.rootInode === current.rootInode &&
        binding.homeDevice === current.homeDevice &&
        binding.homeInode === current.homeInode,
    );
  }

  /** Record a successful content verification at the current binding. */
  record(indexId: string, workspaceRoot: string): void {
    const current = currentWorkspaceBinding(workspaceRoot);
    if (!current) {
      return;
    }
    this.withBindingLock(indexId, () => {
      const bindings = this.readBindings(indexId).filter(
        (binding) => !sameBinding(binding, current),
      );
      bindings.push({ ...current, verifiedTime: Date.now() });
      const trimmed = bindings.slice(-MAX_BINDINGS_PER_INDEX);
      writeJsonFileSync(
        this.recordPath(indexId),
        { version: 1, bindings: trimmed } satisfies BindingRecord,
        { directoryMode: 0o700, fileMode: 0o600 },
      );
      this.evictOldIndexes();
    });
  }

  /**
   * Explicitly drop verification for this workspace path. Called by every
   * supported restore/replacement workflow before publication, and when
   * forced reconciliation starts. Serialized with `record` per index UUID so
   * a concurrent update cannot resurrect an invalidated record. Only a
   * genuinely absent record is ignored; other I/O failures propagate.
   */
  invalidate(indexId: string, workspaceRoot: string): void {
    const rootPath = resolveBindingRoot(workspaceRoot);
    if (rootPath === undefined) {
      return;
    }
    this.withBindingLock(indexId, () => {
      const remaining = this.readBindings(indexId).filter(
        (binding) => binding.rootPath !== rootPath,
      );
      if (remaining.length === 0) {
        try {
          unlinkSync(this.recordPath(indexId));
        } catch (error) {
          if (isNodeError(error) && error.code === "ENOENT") {
            return;
          }
          throw error;
        }
        return;
      }
      writeJsonFileSync(
        this.recordPath(indexId),
        { version: 1, bindings: remaining } satisfies BindingRecord,
        { directoryMode: 0o700, fileMode: 0o600 },
      );
    });
  }

  /**
   * Serialize read-modify-write cycles of the per-UUID record. Binding locks
   * are always taken inside (never around) workspace home locks and
   * destination reservations, keeping a consistent order.
   */
  private withBindingLock<T>(indexId: string, operation: () => T): T {
    if (!/^[A-Za-z0-9_-]+$/.test(indexId)) {
      throw new Error(`Invalid index id for binding record: ${indexId}`);
    }
    const lock = acquireReadWriteLock(
      join(this.home, BINDINGS_DIRECTORY, `${indexId}.lock`),
      "write",
      { operation: "bindings.update" },
    );
    try {
      return operation();
    } finally {
      lock.release();
    }
  }

  private recordPath(indexId: string): string {
    if (!/^[A-Za-z0-9_-]+$/.test(indexId)) {
      throw new Error(`Invalid index id for binding record: ${indexId}`);
    }
    return join(this.home, BINDINGS_DIRECTORY, `${indexId}.json`);
  }

  private readBindings(indexId: string): WorkspaceBinding[] {
    let value: BindingRecord | null = null;
    try {
      value = readJsonFileSync<BindingRecord | null>(
        this.recordPath(indexId),
        null,
      );
    } catch {
      return [];
    }
    if (!value || value.version !== 1 || !Array.isArray(value.bindings)) {
      return [];
    }
    return value.bindings.filter(
      (binding) =>
        typeof binding?.rootPath === "string" &&
        typeof binding.rootDevice === "number" &&
        typeof binding.rootInode === "number" &&
        typeof binding.homeDevice === "number" &&
        typeof binding.homeInode === "number" &&
        typeof binding.verifiedTime === "number",
    );
  }

  private evictOldIndexes(): void {
    // Bounded by index count: removing the oldest record only forces a safe
    // re-verification of that index at its next open.
    try {
      const directory = join(this.home, BINDINGS_DIRECTORY);
      const entries = readdirSync(directory)
        .filter((name) => name.endsWith(".json"))
        .map((name) => {
          const path = join(directory, name);
          return { path, mtimeMs: statSync(path).mtimeMs };
        })
        .sort((left, right) => left.mtimeMs - right.mtimeMs);
      for (const entry of entries.slice(
        0,
        Math.max(0, entries.length - MAX_INDEX_ENTRIES),
      )) {
        unlinkSync(entry.path);
      }
    } catch {
      // Eviction is best-effort; a full store only costs re-verification.
    }
  }
}
