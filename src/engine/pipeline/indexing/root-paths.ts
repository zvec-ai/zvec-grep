import { realpathSync, statSync, type Stats } from "node:fs";
import { dirname, relative } from "node:path";
import { EngineError } from "../../errors/index.js";
import type { RootPath } from "../../types.js";
import { pathPatternMatches } from "../../utils/glob.js";
import {
  isPathInside,
  normalizePath,
  toDisplayPath,
} from "../../utils/path.js";

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
        throw new EngineError("Collection root paths overlap", {
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
  stat: Stats;
};

function rootPathToScanDomain(root: RootPath): RootScanDomain {
  let info: Stats | undefined;

  try {
    info = statSync(root.absolutePath, { throwIfNoEntry: false });
  } catch (cause) {
    throw new EngineError("Collection root path could not be inspected", {
      code: "ZVEC_GREP.ENGINE.SCANNER.ROOT_PATH_STAT_FAILED",
      context: `rootPath=${root.absolutePath}`,
      cause,
    });
  }

  if (!info) {
    throw new EngineError("Collection root path does not exist", {
      code: "ZVEC_GREP.ENGINE.SCANNER.ROOT_PATH_MISSING",
      context: `rootPath=${root.absolutePath}`,
    });
  }

  const kind = info.isFile() ? "file" : info.isDirectory() ? "directory" : null;

  if (!kind) {
    throw new EngineError("Collection root path must be a file or directory", {
      code: "ZVEC_GREP.ENGINE.SCANNER.UNSUPPORTED_ROOT_PATH",
      context: `rootPath=${root.absolutePath}`,
    });
  }

  let realPath: string;

  try {
    realPath = normalizePath(realpathSync(root.absolutePath));
  } catch (cause) {
    throw new EngineError("Collection root path could not be resolved", {
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
      left.stat.ino !== 0 &&
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
