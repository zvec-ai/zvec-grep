import { readdirSync, realpathSync } from "node:fs";
import { readdir } from "node:fs/promises";
import { isAbsolute, join, relative, sep } from "node:path";
import { EngineError } from "../errors.js";
import { sha256Text } from "./hash.js";
import { isPathInside, normalizePath } from "./path.js";

// Canonical workspace-relative paths (CRP) per
// docs/design/portable-workspace-index.md: "/" separator, NFC segments, case
// preserved from the actual directory entry, no "." or ".." segments, no
// backslashes.

const ROOT_CRP = ".";

export function makeFileId(
  workspaceIndexId: string,
  canonicalPath: string,
): string {
  return sha256Text(`${workspaceIndexId}\0${canonicalPath}`);
}

/** NFC-normalize every segment of a "/" separated relative path. */
export function canonicalFromRelative(relativePath: string): string {
  return relativePath
    .split("/")
    .filter((segment) => segment.length > 0 && segment !== ".")
    .map((segment) => segment.normalize("NFC"))
    .join("/");
}

/**
 * Compute a file's CRP from its absolute path. Both inputs must be
 * comparably resolved (realpath-normalized). Returns null when the file is
 * outside the workspace.
 */
export function canonicalRelativePath(
  workspaceRoot: string,
  absolutePath: string,
): string | null {
  const relativePath = relative(
    normalizePath(workspaceRoot),
    normalizePath(absolutePath),
  );
  if (
    relativePath.length === 0 ||
    isAbsolute(relativePath) ||
    relativePath === ".." ||
    relativePath.startsWith(`..${sep}`)
  ) {
    return null;
  }
  return canonicalFromRelative(relativePath.split(sep).join("/"));
}

/** The CRP used for the workspace root itself in root-path configuration. */
export function workspaceRootCrp(): string {
  return ROOT_CRP;
}

/**
 * True when the value is a structurally valid CRP (form only). Backslashes
 * are rejected so stored portable data cannot change meaning across
 * platforms' separators.
 */
export function isCanonicalRelativePath(value: string): boolean {
  if (value === ROOT_CRP) {
    return true;
  }
  if (value.length === 0 || value.startsWith("/") || value.endsWith("/")) {
    return false;
  }
  return value
    .split("/")
    .every(
      (segment) =>
        segment.length > 0 &&
        segment !== "." &&
        segment !== ".." &&
        !segment.includes("\\") &&
        !segment.includes("\0"),
    );
}

export type CanonicalNameCollision = {
  kind: "unicode" | "case";
  directory: string;
  names: string[];
};

/** Detect NFC and case-folded collisions within one directory listing. */
export function findCanonicalNameCollisions(
  directory: string,
  names: readonly string[],
): CanonicalNameCollision[] {
  const collisions: CanonicalNameCollision[] = [];
  for (const kind of ["unicode", "case"] as const) {
    const seen = new Map<string, string[]>();
    for (const name of names) {
      const key =
        kind === "unicode"
          ? name.normalize("NFC")
          : name.normalize("NFC").toLowerCase();
      const group = seen.get(key);
      if (group) {
        if (!group.includes(name)) {
          group.push(name);
        }
      } else {
        seen.set(key, [name]);
      }
    }
    for (const group of seen.values()) {
      if (group.length > 1) {
        collisions.push({ kind, directory, names: group });
      }
    }
  }
  return collisions;
}

export class CanonicalPathResolutionError extends EngineError {
  constructor(message: string, context: string) {
    super(message, {
      code: "ZVEC_GREP.ENGINE.PATHS.CANONICAL_RESOLUTION_FAILED",
      context,
    });
  }
}

/**
 * Tri-state resolution result. "missing" means no entry exists at the
 * canonical location; "forbidden" means an entry exists but escapes the
 * workspace through a symlink (including any intermediate component). A
 * forbidden result is never a missing file: callers must not fall back to
 * reconstructed pathnames for it.
 */
export type CanonicalResolution =
  | { status: "ok"; path: string }
  | { status: "missing" }
  | { status: "forbidden"; path: string };

type DirectoryEntries = Map<string, string[]>;

/**
 * Resolver mapping CRPs to current absolute paths through per-segment
 * actual-name lookup (NFC match) with real containment enforcement. Results
 * are cached per directory for the lifetime of the resolver; create one per
 * operation/session.
 */
export type CanonicalPathResolver = {
  readonly workspaceRoot: string;
  /** Real path of the workspace root, used for containment checks. */
  readonly workspaceRealRoot: string;
  resolveDetailedSync(canonicalPath: string): CanonicalResolution;
  resolveDetailed(canonicalPath: string): Promise<CanonicalResolution>;
  /** The CRP of an absolute path inside this workspace, or null outside. */
  toCanonical(absolutePath: string): string | null;
  /** Throw when the CRP does not resolve to a contained existing path. */
  requireContainedSync(canonicalPath: string): string;
};

export function tryRealpathSync(path: string): string | undefined {
  try {
    // Use the same physical spelling as fs.promises.realpath in the daemon
    // and scanner. Windows' JS resolver can retain an 8.3 path alias.
    return realpathSync.native(path);
  } catch {
    return undefined;
  }
}

function isAbsenceError(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    ((error as { code?: string }).code === "ENOENT" ||
      (error as { code?: string }).code === "ENOTDIR")
  );
}

function errorCode(error: unknown): string {
  return typeof error === "object" && error !== null && "code" in error
    ? String((error as { code?: string }).code)
    : "unknown";
}

export function createCanonicalPathResolver(
  workspaceRoot: string,
): CanonicalPathResolver {
  const root = normalizePath(workspaceRoot);
  const realRoot = tryRealpathSync(root) ?? root;
  const cache = new Map<string, DirectoryEntries>();

  function entriesFor(directory: string): DirectoryEntries {
    const cached = cache.get(directory);
    if (cached) {
      return cached;
    }
    const entries: DirectoryEntries = new Map();
    let names: string[] = [];
    try {
      names = readdirSync(directory);
    } catch (error) {
      if (!isAbsenceError(error)) {
        throw new CanonicalPathResolutionError(
          "Filesystem error while reading a directory",
          `directory=${directory} code=${errorCode(error)}`,
        );
      }
      // Genuine absence only: every segment misses.
    }
    for (const name of names) {
      const key = name.normalize("NFC");
      const group = entries.get(key);
      if (group) {
        group.push(name);
      } else {
        entries.set(key, [name]);
      }
    }
    cache.set(directory, entries);
    return entries;
  }

  function select(
    directory: string,
    segment: string,
    entries: DirectoryEntries,
  ): string | null {
    const matches = entries.get(segment.normalize("NFC")) ?? [];
    if (matches.length === 0) {
      return null;
    }
    if (matches.length > 1) {
      throw new CanonicalPathResolutionError(
        "Canonical path segment is ambiguous on this filesystem",
        `directory=${directory} segment=${segment} matches=${matches.join(",")}`,
      );
    }
    return join(directory, matches[0]);
  }

  function assertValid(canonicalPath: string): void {
    if (!isCanonicalRelativePath(canonicalPath)) {
      throw new CanonicalPathResolutionError(
        "Stored canonical path is invalid",
        `canonicalPath=${canonicalPath}`,
      );
    }
  }

  function finish(path: string): CanonicalResolution {
    const realPath = realpathForResolve(path);
    if (realPath === undefined) {
      return { status: "missing" };
    }
    if (!isContained(realPath)) {
      return { status: "forbidden", path };
    }
    return { status: "ok", path };
  }

  // Genuine absence (ENOENT/ENOTDIR) resolves to undefined; permission and
  // I/O failures are explicit errors, never missing-path evidence.
  function realpathForResolve(path: string): string | undefined {
    try {
      return realpathSync.native(path);
    } catch (error) {
      if (isAbsenceError(error)) {
        return undefined;
      }
      throw new CanonicalPathResolutionError(
        "Filesystem error while resolving a canonical path",
        `path=${path} code=${errorCode(error)}`,
      );
    }
  }

  // Containment of every existing traversed component is checked before
  // descending or returning a missing result: an escaping intermediate
  // symlink is always forbidden, never a missing file.
  const containmentCache = new Map<string, boolean>();
  function isContained(realPath: string): boolean {
    const cached = containmentCache.get(realPath);
    if (cached !== undefined) {
      return cached;
    }
    const contained = isPathInside(realRoot, realPath);
    containmentCache.set(realPath, contained);
    return contained;
  }

  function descend(
    current: string,
    next: string | null,
  ):
    | { action: "missing" }
    | { action: "forbidden"; path: string }
    | { action: "ok"; path: string } {
    if (next === null) {
      return { action: "missing" };
    }
    const realNext = realpathForResolve(next);
    if (realNext !== undefined && !isContained(realNext)) {
      return { action: "forbidden", path: next };
    }
    return { action: "ok", path: next };
  }

  function resolveDetailedSync(canonicalPath: string): CanonicalResolution {
    assertValid(canonicalPath);
    if (canonicalPath === ROOT_CRP) {
      return { status: "ok", path: root };
    }
    let current = root;
    for (const segment of canonicalPath.split("/")) {
      const step = descend(
        current,
        select(current, segment, entriesFor(current)),
      );
      if (step.action !== "ok") {
        return step.action === "missing"
          ? { status: "missing" }
          : { status: "forbidden", path: step.path };
      }
      current = step.path;
    }
    return finish(current);
  }

  async function resolveDetailed(
    canonicalPath: string,
  ): Promise<CanonicalResolution> {
    assertValid(canonicalPath);
    if (canonicalPath === ROOT_CRP) {
      return { status: "ok", path: root };
    }
    let current = root;
    for (const segment of canonicalPath.split("/")) {
      let entries = cache.get(current);
      if (!entries) {
        let names: string[] = [];
        try {
          names = await readdir(current);
        } catch (error) {
          if (!isAbsenceError(error)) {
            throw new CanonicalPathResolutionError(
              "Filesystem error while reading a directory",
              `directory=${current} code=${errorCode(error)}`,
            );
          }
          // Genuine absence only: every segment misses.
        }
        entries = new Map();
        for (const name of names) {
          const key = name.normalize("NFC");
          const group = entries.get(key);
          if (group) {
            group.push(name);
          } else {
            entries.set(key, [name]);
          }
        }
        cache.set(current, entries);
      }
      const step = descend(current, select(current, segment, entries));
      if (step.action !== "ok") {
        return step.action === "missing"
          ? { status: "missing" }
          : { status: "forbidden", path: step.path };
      }
      current = step.path;
    }
    return finish(current);
  }

  return {
    workspaceRoot: root,
    workspaceRealRoot: realRoot,
    resolveDetailedSync,
    resolveDetailed,
    toCanonical: (absolutePath) => canonicalRelativePath(root, absolutePath),
    requireContainedSync(canonicalPath: string): string {
      const resolution = resolveDetailedSync(canonicalPath);
      if (resolution.status === "ok") {
        return resolution.path;
      }
      throw new CanonicalPathResolutionError(
        resolution.status === "forbidden"
          ? "Canonical path escapes the workspace"
          : "Canonical path does not exist",
        `canonicalPath=${canonicalPath} status=${resolution.status} workspaceRoot=${root}`,
      );
    },
  };
}
