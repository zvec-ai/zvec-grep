import { realpathSync, statSync, type BigIntStats } from "node:fs";
import { basename, dirname, isAbsolute, join, relative } from "node:path";
import { EngineError } from "../../errors.js";
import type { WorkspaceManifestRootPath } from "../../manifest.js";
import type { RootPath } from "../../types.js";
import {
  canonicalFromRelative,
  tryRealpathSync,
  workspaceRootCrp,
  type CanonicalPathResolver,
} from "../../utils/canonical-path.js";
import { pathPatternMatches } from "../../utils/glob.js";
import {
  isPathInside,
  normalizePath,
  toDisplayPath,
} from "../../utils/path.js";

/**
 * Convert runtime roots to their persisted manifest form: canonical
 * workspace-relative paths with selection options, no absolute locations.
 * Ignore-file references are canonicalized too and must stay inside the
 * workspace; anything else is an explicit error.
 */
export function manifestRootPathsFromRuntime(
  paths: readonly RootPath[],
  resolver: CanonicalPathResolver,
): WorkspaceManifestRootPath[] {
  return canonicalizeRootPaths(paths, resolver).map((root) => {
    const { absolutePath: _absolutePath, canonicalPath, ...options } = root;
    return {
      ...options,
      path: canonicalPath as string,
      ...(root.ignoreFiles !== undefined
        ? { ignoreFiles: portableIgnoreFiles(root, resolver) }
        : {}),
    };
  });
}

function portableIgnoreFiles(
  root: RootPath,
  resolver: CanonicalPathResolver,
): string[] {
  return (root.ignoreFiles ?? []).map((entry) => {
    const absolutePath = normalizePath(
      isAbsolute(entry) ? entry : join(root.absolutePath, entry),
    );
    const canonicalPath =
      resolver.toCanonical(absolutePath) ??
      canonicalFilePathWithAlias(absolutePath, resolver);
    if (canonicalPath === null) {
      throw new EngineError("Configured ignore file is outside the workspace", {
        code: "ZVEC_GREP.ENGINE.SCANNER.IGNORE_FILE_OUTSIDE_WORKSPACE",
        context: `ignoreFile=${entry} root=${root.absolutePath} workspaceRoot=${resolver.workspaceRoot}`,
      });
    }
    return canonicalPath;
  });
}

/**
 * CRP of a workspace file addressed through an equivalent alias spelling
 * of the workspace. Unlike a root, a file is never the workspace root
 * itself, so an empty alias suffix is outside the file namespace.
 */
function canonicalFilePathWithAlias(
  absolutePath: string,
  resolver: CanonicalPathResolver,
): string | null {
  const aliasSuffix = workspaceAliasSuffix(absolutePath, resolver);
  if (aliasSuffix === null || aliasSuffix.length === 0) {
    return null;
  }
  return canonicalFromRelative(aliasSuffix.join("/"));
}

/**
 * A changed path's spelling inside the workspace: unchanged when it
 * already matches the resolver's workspace root, remapped through the
 * workspace alias boundary when it addresses the same physical tree
 * through an equivalent spelling (macOS /var, Windows short names), and
 * left as given when it is genuinely outside — downstream handling
 * decides that case. Watcher events and caller-supplied changed paths
 * may carry either spelling. An empty logical suffix is the workspace
 * root itself — a notification to rescan the workspace directory — and
 * maps to the resolver's workspace root; nonempty suffixes keep their
 * logical identity even when an internal link resolves to the root.
 * Deleted targets map through their canonical identity without
 * requiring the leaf or subtree to exist: absence is the notification's
 * point, and stored-entry comparison and removal need the workspace
 * spelling. Escaping symlinks surface as forbidden and are left to
 * containment; ambiguous canonical names and filesystem errors still
 * throw.
 */
export function resolveWorkspaceFilePath(
  absolutePath: string,
  resolver: CanonicalPathResolver,
): string {
  if (resolver.toCanonical(absolutePath) !== null) {
    return absolutePath;
  }
  const aliasSuffix = workspaceAliasSuffix(absolutePath, resolver);
  if (aliasSuffix === null) {
    return absolutePath;
  }
  if (aliasSuffix.length === 0) {
    return resolver.workspaceRoot;
  }
  const canonicalPath = canonicalFromRelative(aliasSuffix.join("/"));
  const resolution = resolver.resolveDetailedSync(canonicalPath);
  if (resolution.status === "ok") {
    return resolution.path;
  }
  if (resolution.status === "missing") {
    return join(resolver.workspaceRoot, canonicalPath);
  }
  return absolutePath;
}
/**
 * Assign each root its canonical workspace-relative path, the identity
 * reference point for scanned files. Roots must stay inside the workspace,
 * including through symlinks; anything else is an explicit error in portable
 * mode.
 */
export function canonicalizeRootPaths(
  paths: readonly RootPath[],
  resolver: CanonicalPathResolver,
): RootPath[] {
  return paths.map((root) => {
    const realRoot = tryRealpathSync(root.absolutePath);
    const canonicalPath =
      root.canonicalPath ?? canonicalRootPath(root, resolver);
    const escapes =
      realRoot !== undefined &&
      !isPathInside(resolver.workspaceRealRoot, realRoot);
    if (canonicalPath === null || escapes) {
      throw new EngineError(
        "Workspace index root path is outside the workspace",
        {
          code: "ZVEC_GREP.ENGINE.SCANNER.ROOT_PATH_OUTSIDE_WORKSPACE",
          context: `rootPath=${root.absolutePath} workspaceRoot=${resolver.workspaceRoot}`,
        },
      );
    }
    return { ...root, canonicalPath };
  });
}

/**
 * The CRP of a runtime root. Textual spelling decides first; when it does
 * not match, an equivalent spelling of the workspace decides: a workspace
 * may be addressed through a symlinked alias (macOS /var vs /private/var),
 * but only the alias prefix is normalized — the segments below the
 * workspace root keep their logical spelling so an internal symlink does
 * not rename the selection. Selections with no prefix physically equal to
 * the workspace root yield null; resolved-root containment stays with the
 * caller's escape check.
 */
function canonicalRootPath(
  root: RootPath,
  resolver: CanonicalPathResolver,
): string | null {
  if (normalizePath(root.absolutePath) === resolver.workspaceRoot) {
    return workspaceRootCrp();
  }
  const textual = resolver.toCanonical(root.absolutePath);
  if (textual !== null) {
    return textual;
  }
  const aliasSuffix = workspaceAliasSuffix(root.absolutePath, resolver);
  if (aliasSuffix === null) {
    return null;
  }
  return aliasSuffix.length === 0
    ? workspaceRootCrp()
    : canonicalFromRelative(aliasSuffix.join("/"));
}

/**
 * The workspace-relative suffix of a path addressed through an equivalent
 * spelling of the workspace: the outermost prefix that physically resolves
 * to the workspace root marks the alias boundary. Nearer matches are
 * internal links back to the workspace root and belong to the logical
 * suffix, not to the boundary.
 */
function workspaceAliasSuffix(
  absolutePath: string,
  resolver: CanonicalPathResolver,
): string[] | null {
  const suffix: string[] = [];
  let current = normalizePath(absolutePath);
  let outermost: string[] | null = null;
  while (true) {
    if (tryRealpathSync(current) === resolver.workspaceRealRoot) {
      outermost = [...suffix].reverse();
    }
    const parent = dirname(current);
    if (parent === current) {
      return outermost;
    }
    suffix.push(basename(current));
    current = parent;
  }
}

export function validateRootPaths(
  paths: readonly (string | RootPath)[],
): RootPath[] {
  const roots = paths.map(normalizeRootPath);
  const domains = roots.map(rootPathToScanDomain);

  for (let leftIndex = 0; leftIndex < domains.length; leftIndex++) {
    for (
      let rightIndex = leftIndex + 1;
      rightIndex < domains.length;
      rightIndex++
    ) {
      const left = domains[leftIndex];
      const right = domains[rightIndex];

      if (scanDomainsOverlap(left, right)) {
        throw new EngineError("Workspace index root paths overlap", {
          code: "ZVEC_GREP.ENGINE.SCANNER.OVERLAPPING_ROOT_PATHS",
          context: `left=${left.root.absolutePath} right=${right.root.absolutePath}`,
        });
      }
    }
  }

  return roots;
}

export function normalizeRootPath(path: string | RootPath): RootPath {
  if (typeof path === "string") {
    return {
      absolutePath: normalizePath(path),
      recursive: true,
    };
  }

  return {
    ...path,
    absolutePath: normalizePath(path.absolutePath),
    recursive: path.recursive,
  };
}

export function fileBelongsToRootPath(
  absolutePath: string,
  rootPath: RootPath,
): boolean {
  const normalizedPath = normalizePath(absolutePath);

  if (!isPathInside(rootPath.absolutePath, normalizedPath)) {
    return false;
  }

  const relativePath = toDisplayPath(
    relative(rootPath.absolutePath, normalizedPath),
  );

  return matchesRootPatterns(relativePath, rootPath);
}

export function matchesRootPatterns(
  relativePath: string,
  rootPath: RootPath,
): boolean {
  if (matchesAny(relativePath, rootPath.exclude)) {
    return false;
  }

  if (!rootPath.include || rootPath.include.length === 0) {
    return true;
  }

  return matchesAny(relativePath, rootPath.include);
}

export function matchesRootIncludePatterns(
  relativePath: string,
  rootPath: RootPath,
): boolean {
  return matchesAny(relativePath, rootPath.include);
}

export function matchesRootExcludePatterns(
  relativePath: string,
  rootPath: RootPath,
): boolean {
  return matchesAny(relativePath, rootPath.exclude);
}

type RootScanDomain = {
  root: RootPath;
  realPath: string;
  kind: "file" | "directory";
  stat: BigIntStats;
};

function rootPathToScanDomain(root: RootPath): RootScanDomain {
  let info: BigIntStats | undefined;

  try {
    // Preserve 64-bit file identities that cannot be represented exactly as Numbers.
    info = statSync(root.absolutePath, { bigint: true, throwIfNoEntry: false });
  } catch (cause) {
    throw new EngineError("Workspace index root path could not be inspected", {
      code: "ZVEC_GREP.ENGINE.SCANNER.ROOT_PATH_STAT_FAILED",
      context: `rootPath=${root.absolutePath}`,
      cause,
    });
  }

  if (!info) {
    throw new EngineError("Workspace index root path does not exist", {
      code: "ZVEC_GREP.ENGINE.SCANNER.ROOT_PATH_MISSING",
      context: `rootPath=${root.absolutePath}`,
    });
  }

  const kind = info.isFile() ? "file" : info.isDirectory() ? "directory" : null;

  if (!kind) {
    throw new EngineError(
      "Workspace index root path must be a file or directory",
      {
        code: "ZVEC_GREP.ENGINE.SCANNER.UNSUPPORTED_ROOT_PATH",
        context: `rootPath=${root.absolutePath}`,
      },
    );
  }

  let realPath: string;

  try {
    realPath = normalizePath(realpathSync.native(root.absolutePath));
  } catch (cause) {
    throw new EngineError("Workspace index root path could not be resolved", {
      code: "ZVEC_GREP.ENGINE.SCANNER.ROOT_PATH_REALPATH_FAILED",
      context: `rootPath=${root.absolutePath}`,
      cause,
    });
  }

  return {
    root,
    realPath,
    kind,
    stat: info,
  };
}

function scanDomainsOverlap(
  left: RootScanDomain,
  right: RootScanDomain,
): boolean {
  if (sameFileIdentity(left, right)) {
    return true;
  }

  if (left.kind === "file" && right.kind === "file") {
    return false;
  }

  if (left.kind === "directory" && right.kind === "directory") {
    return directoryDomainsOverlap(left, right);
  }

  const directory = left.kind === "directory" ? left : right;
  const file = left.kind === "file" ? left : right;

  return directoryCoversFile(directory, file.realPath);
}

function sameFileIdentity(
  left: RootScanDomain,
  right: RootScanDomain,
): boolean {
  return (
    left.realPath === right.realPath ||
    (left.stat.dev === right.stat.dev &&
      left.stat.ino !== 0n &&
      left.stat.ino === right.stat.ino)
  );
}

function directoryDomainsOverlap(
  left: RootScanDomain,
  right: RootScanDomain,
): boolean {
  if (left.realPath === right.realPath) {
    return true;
  }

  return (
    directoryCoversDirectory(left, right.realPath) ||
    directoryCoversDirectory(right, left.realPath)
  );
}

function directoryCoversDirectory(
  directory: RootScanDomain,
  childDirectoryPath: string,
): boolean {
  return (
    directory.root.recursive &&
    isPathInside(directory.realPath, childDirectoryPath)
  );
}

function directoryCoversFile(
  directory: RootScanDomain,
  filePath: string,
): boolean {
  if (!isPathInside(directory.realPath, filePath)) {
    return false;
  }

  return directory.root.recursive || dirname(filePath) === directory.realPath;
}

function matchesAny(
  relativePath: string,
  patterns: readonly string[] | undefined,
): boolean {
  if (!patterns || patterns.length === 0) {
    return false;
  }

  return patterns.some((pattern) => patternMatches(pattern, relativePath));
}

function patternMatches(pattern: string, relativePath: string): boolean {
  return pathPatternMatches(pattern, relativePath);
}
