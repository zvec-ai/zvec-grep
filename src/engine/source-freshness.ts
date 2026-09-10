import { readFileSync, statSync } from "node:fs";
import { isAbsolute } from "node:path";
import { EngineError } from "./errors.js";
import { pathsForConfiguredRoots } from "./pipeline/indexing/root-paths.js";
import type { WorkspaceIndexStorage } from "./storage/index.js";
import type {
  FileInfo,
  IndexedSourceVersion,
  SourceInvalidation,
  SourceInvalidationReason,
  WorkspaceIndexInfo,
  WorkspaceSourceFreshnessProof,
} from "./types.js";
import { sha256Bytes } from "./utils/hash.js";
import { isPathInside, normalizePath } from "./utils/path.js";

export type IndexedSourceFreshness =
  | { status: "fresh"; indexed: IndexedSourceVersion }
  | { status: "possibly_stale"; invalidation: SourceInvalidation };

export const MAX_CHANGED_SOURCE_HASH_BYTES = 16 * 1024 * 1024;

export function indexedSourceVersion(
  workspaceIndexId: string,
  file: FileInfo,
): IndexedSourceVersion {
  return {
    workspaceIndexId,
    fileId: file.id,
    absolutePath: normalizePath(file.absolutePath),
    rootPath: file.rootPath,
    indexedTime: file.indexStatus?.indexedTime ?? null,
    ...(file.contentHash === undefined
      ? {}
      : { contentHash: file.contentHash }),
    sizeBytes: file.sizeBytes,
  };
}

/** Verify the source bytes, not its mtime or the index's later commit time. */
export function inspectIndexedSource(
  workspaceIndexId: string,
  file: FileInfo,
): IndexedSourceFreshness {
  const indexed = indexedSourceVersion(workspaceIndexId, file);
  const invalid = (
    reason: SourceInvalidationReason,
    observed?: { observedHash?: string; observedSizeBytes: number },
  ): IndexedSourceFreshness => ({
    status: "possibly_stale",
    invalidation: { indexed, reason, ...observed },
  });
  try {
    const info = statSync(indexed.absolutePath);
    if (!info.isFile()) return invalid("not_file");
    if (
      info.size !== indexed.sizeBytes &&
      info.size > MAX_CHANGED_SOURCE_HASH_BYTES
    ) {
      // Growing a previously small file must not force an unbounded read merely
      // to distinguish invalidation versions. Size alone proves it is stale.
      return invalid("size_mismatch", { observedSizeBytes: info.size });
    }
    // Hash even an obvious size mismatch: two subsequent edits may have the
    // same size, so an observed hash is needed to distinguish their evidence.
    const bytes = readFileSync(indexed.absolutePath);
    const observed = {
      observedHash: sha256Bytes(bytes),
      observedSizeBytes: bytes.byteLength,
    };
    if (
      indexed.indexedTime === null ||
      !indexed.contentHash ||
      file.indexStatus?.error !== undefined
    ) {
      return invalid("unverified", observed);
    }
    if (indexed.sizeBytes !== observed.observedSizeBytes) {
      return invalid("size_mismatch", observed);
    }
    if (indexed.contentHash !== observed.observedHash) {
      return invalid("hash_mismatch", observed);
    }
    return { status: "fresh", indexed };
  } catch (error) {
    return invalid(isMissing(error) ? "missing" : "unreadable");
  }
}

/**
 * Read exact committed records from the caller's current storage generation.
 * Call after writes have finalized, while that generation is still leased.
 * A proof describes the bytes observed now, not a promise against future edits.
 */
export async function verifyWorkspaceSourceFreshness(
  workspaceIndex: WorkspaceIndexInfo,
  storage: Pick<WorkspaceIndexStorage, "getFileByPath">,
  paths: readonly string[],
  signal?: AbortSignal,
): Promise<WorkspaceSourceFreshnessProof> {
  const targets = snapshotSourceVerificationPaths(workspaceIndex, paths);
  const proofs: WorkspaceSourceFreshnessProof["paths"][number][] = [];
  signal?.throwIfAborted();
  for (const absolutePath of targets) {
    signal?.throwIfAborted();
    const file = storage.getFileByPath(absolutePath);
    if (file) {
      const inspected = inspectIndexedSource(workspaceIndex.id, file);
      proofs.push(
        inspected.status === "fresh"
          ? { absolutePath, status: "fresh", indexed: inspected.indexed }
          : {
              absolutePath,
              status: "unverified",
              reason: inspected.invalidation.reason,
              invalidation: inspected.invalidation,
            },
      );
      continue;
    }
    // A caller cannot turn a missing storage record into arbitrary filesystem
    // reads outside the index's configured roots.
    if (
      !workspaceIndex.rootPaths.some((root) =>
        isPathInside(root.absolutePath, absolutePath),
      )
    ) {
      proofs.push({
        absolutePath,
        status: "unverified",
        reason: "out_of_scope",
      });
      continue;
    }
    try {
      statSync(absolutePath);
      // In particular, a file can reappear after indexing deleted it. Absence
      // from storage alone is not proof that this path has been reconciled.
      proofs.push({
        absolutePath,
        status: "unverified",
        reason: "not_indexed",
      });
    } catch (error) {
      proofs.push(
        isMissing(error)
          ? { absolutePath, status: "absent" }
          : { absolutePath, status: "unverified", reason: "unreadable" },
      );
    }
  }
  signal?.throwIfAborted();
  return { workspaceIndexId: workspaceIndex.id, paths: proofs };
}

export function snapshotSourceVerificationPaths(
  workspaceIndex: Pick<WorkspaceIndexInfo, "rootPaths">,
  paths: readonly string[],
): string[] {
  const snapshot = [...paths];
  for (const path of snapshot) {
    if (!isAbsolute(path)) {
      throw new EngineError("Source verification paths must be absolute", {
        code: "ZVEC_GREP.ENGINE.FRESHNESS.INVALID_PATH",
      });
    }
  }
  return [
    ...new Set(
      snapshot.flatMap((path) =>
        pathsForConfiguredRoots(workspaceIndex.rootPaths, path).map(
          normalizePath,
        ),
      ),
    ),
  ];
}

function isMissing(error: unknown): boolean {
  const code = (error as NodeJS.ErrnoException | undefined)?.code;
  return code === "ENOENT" || code === "ENOTDIR";
}
