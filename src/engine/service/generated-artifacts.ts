import { readFile, realpath, stat } from "node:fs/promises";
import { isAbsolute, relative, resolve, sep } from "node:path";
import { sha256Bytes } from "../utils/hash.js";
import { isPathInside, normalizePath, toDisplayPath } from "../utils/path.js";
import type { ZvecGrepContextFile, ZvecGrepContextItem } from "./types.js";

const GENERATED_DIRECTORY_NAMES = new Set([
  ".next",
  ".nuxt",
  ".output",
  "__generated__",
  "build",
  "coverage",
  "dist",
  "gen",
  "generated",
  "out",
  "target",
]);

export type CanonicalizeGeneratedArtifactMatchesOptions = {
  root: string;
  items: readonly ZvecGrepContextItem[];
  paths?: readonly string[];
  includePaths?: readonly string[];
  globs?: readonly string[];
  insensitiveGlobs?: readonly string[];
};

export type GeneratedArtifactDiagnostics = {
  generatedCandidates: number;
  generatedMirrorsCanonicalized: number;
  generatedMatchesDemoted: number;
};

export type CanonicalizeGeneratedArtifactMatchesResult = {
  items: ZvecGrepContextItem[];
  diagnostics: GeneratedArtifactDiagnostics;
  demotedGeneratedPaths: ReadonlySet<string>;
};

type ClassifiedItem = {
  item: ZvecGrepContextItem;
  relativePath: string | null;
  generated: boolean;
};

type SourceCandidate = {
  absolutePath: string;
  file: ZvecGrepContextFile;
};

type FileFingerprint = {
  size: number;
  sha256: string;
};

/**
 * Canonicalize rg matches from known generated-output directories when their
 * contents can be proven to be an exact mirror of a source file. Generated
 * matches without a proven source counterpart are retained and stably moved
 * behind ordinary source matches.
 */
export async function canonicalizeGeneratedArtifactMatches(
  options: CanonicalizeGeneratedArtifactMatchesOptions,
): Promise<CanonicalizeGeneratedArtifactMatchesResult> {
  const root = normalizePath(options.root);
  const classified = options.items.map((item): ClassifiedItem => {
    const relativePath = relativePathWithinRoot(root, item.file.absolutePath);
    return {
      item,
      relativePath,
      generated: relativePath !== null && isGeneratedRelativePath(relativePath),
    };
  });
  const generatedCandidates = classified.filter(
    (entry) => entry.generated,
  ).length;
  const emptyDiagnostics = {
    generatedCandidates,
    generatedMirrorsCanonicalized: 0,
    generatedMatchesDemoted: 0,
  };

  if (
    generatedCandidates === 0 ||
    explicitlyTargetsGeneratedDirectories(options, root)
  ) {
    return {
      items: [...options.items],
      diagnostics: emptyDiagnostics,
      demotedGeneratedPaths: new Set(),
    };
  }

  const rootRealPath = await realpath(root).catch(() => root);
  const fingerprintCache = new Map<string, Promise<FileFingerprint | null>>();
  const fingerprint = (path: string): Promise<FileFingerprint | null> => {
    const absolutePath = normalizePath(path);
    const cached = fingerprintCache.get(absolutePath);
    if (cached) {
      return cached;
    }

    const pending = fingerprintFile(root, rootRealPath, absolutePath);
    fingerprintCache.set(absolutePath, pending);
    return pending;
  };

  const sourceCandidates = uniqueSourceCandidates(classified);
  const sourceByAbsolutePath = new Map(
    sourceCandidates.map((candidate) => [
      normalizePath(candidate.absolutePath),
      candidate,
    ]),
  );
  const canonicalFileCache = new Map<
    string,
    Promise<ZvecGrepContextFile | null>
  >();

  const primary: ZvecGrepContextItem[] = [];
  const demoted: ZvecGrepContextItem[] = [];
  const demotedGeneratedPaths = new Set<string>();
  let generatedMirrorsCanonicalized = 0;

  for (const entry of classified) {
    if (!entry.generated || entry.relativePath === null) {
      primary.push(entry.item);
      continue;
    }

    const generatedAbsolutePath = normalizePath(entry.item.file.absolutePath);
    let canonicalFile = canonicalFileCache.get(generatedAbsolutePath);
    if (!canonicalFile) {
      canonicalFile = findCanonicalSourceFile({
        root,
        generatedFile: entry.item.file,
        generatedRelativePath: entry.relativePath,
        sourceByAbsolutePath,
        fingerprint,
      });
      canonicalFileCache.set(generatedAbsolutePath, canonicalFile);
    }

    const sourceFile = await canonicalFile;
    if (sourceFile) {
      generatedMirrorsCanonicalized++;
      primary.push({
        ...entry.item,
        file: sourceFile,
      });
    } else {
      demoted.push(entry.item);
      demotedGeneratedPaths.add(generatedAbsolutePath);
    }
  }

  const items = [...primary, ...demoted].map((item, index) => ({
    ...item,
    rank: index + 1,
  }));

  return {
    items,
    diagnostics: {
      generatedCandidates,
      generatedMirrorsCanonicalized,
      generatedMatchesDemoted: demoted.length,
    },
    demotedGeneratedPaths,
  };
}

async function findCanonicalSourceFile(options: {
  root: string;
  generatedFile: ZvecGrepContextFile;
  generatedRelativePath: string;
  sourceByAbsolutePath: ReadonlyMap<string, SourceCandidate>;
  fingerprint(path: string): Promise<FileFingerprint | null>;
}): Promise<ZvecGrepContextFile | null> {
  const generatedFingerprint = await options.fingerprint(
    options.generatedFile.absolutePath,
  );
  if (!generatedFingerprint) {
    return null;
  }

  for (const candidatePath of safeStripPrefixCandidates(
    options.root,
    options.generatedRelativePath,
  )) {
    // A source is eligible only when the same scoped rg invocation returned
    // that exact path. This preserves exclude/time/ignore semantics without
    // having to reimplement every ripgrep filter here.
    const matchedSource = options.sourceByAbsolutePath.get(
      normalizePath(candidatePath),
    );
    if (!matchedSource) {
      continue;
    }

    const candidateFingerprint = await options.fingerprint(candidatePath);
    if (
      candidateFingerprint &&
      fingerprintsEqual(generatedFingerprint, candidateFingerprint)
    ) {
      return matchedSource.file;
    }
  }

  return null;
}

function uniqueSourceCandidates(
  items: readonly ClassifiedItem[],
): SourceCandidate[] {
  const seen = new Set<string>();
  const candidates: SourceCandidate[] = [];

  for (const entry of items) {
    if (entry.generated || entry.relativePath === null) {
      continue;
    }

    const absolutePath = normalizePath(entry.item.file.absolutePath);
    if (seen.has(absolutePath)) {
      continue;
    }

    seen.add(absolutePath);
    candidates.push({
      absolutePath,
      file: entry.item.file,
    });
  }

  return candidates;
}

async function fingerprintFile(
  root: string,
  rootRealPath: string,
  path: string,
): Promise<FileFingerprint | null> {
  if (!isPathInside(root, path)) {
    return null;
  }

  try {
    const [fileInfo, actualPath] = await Promise.all([
      stat(path),
      realpath(path),
    ]);
    if (!fileInfo.isFile() || !isPathInside(rootRealPath, actualPath)) {
      return null;
    }

    const bytes = await readFile(path);
    return {
      size: bytes.byteLength,
      sha256: sha256Bytes(bytes),
    };
  } catch {
    return null;
  }
}

function fingerprintsEqual(
  left: FileFingerprint,
  right: FileFingerprint,
): boolean {
  return left.size === right.size && left.sha256 === right.sha256;
}

function safeStripPrefixCandidates(
  root: string,
  relativePath: string,
): string[] {
  const segments = pathSegments(relativePath);
  const candidates: string[] = [];
  const seen = new Set<string>();

  for (let index = 0; index < segments.length - 1; index++) {
    if (!isGeneratedDirectoryName(segments[index]!)) {
      continue;
    }

    const removalLengths =
      segments[index]!.toLowerCase() === "build" &&
      segments[index + 1]?.toLowerCase() === "lib"
        ? [2, 1]
        : [1];

    for (const removalLength of removalLengths) {
      const stripped = [
        ...segments.slice(0, index),
        ...segments.slice(index + removalLength),
      ];
      if (
        stripped.length === 0 ||
        stripped.some((segment) => isGeneratedDirectoryName(segment))
      ) {
        continue;
      }

      const absolutePath = resolve(root, ...stripped);
      if (
        !isPathInside(root, absolutePath) ||
        absolutePath === root ||
        seen.has(absolutePath)
      ) {
        continue;
      }

      seen.add(absolutePath);
      candidates.push(absolutePath);
    }
  }

  return candidates;
}

function relativePathWithinRoot(
  root: string,
  absolutePath: string,
): string | null {
  const normalizedPath = normalizePath(absolutePath);
  if (!isPathInside(root, normalizedPath)) {
    return null;
  }

  const relativePath = relative(root, normalizedPath);
  return relativePath.length > 0 ? toDisplayPath(relativePath) : null;
}

function isGeneratedRelativePath(path: string): boolean {
  const segments = pathSegments(path);
  return segments
    .slice(0, -1)
    .some((segment) => isGeneratedDirectoryName(segment));
}

function isGeneratedDirectoryName(segment: string): boolean {
  return GENERATED_DIRECTORY_NAMES.has(segment.toLowerCase());
}

function pathSegments(path: string): string[] {
  return path
    .split(/[\\/]+/)
    .map((segment) => segment.trim())
    .filter((segment) => segment.length > 0 && segment !== ".");
}

function explicitlyTargetsGeneratedDirectories(
  options: CanonicalizeGeneratedArtifactMatchesOptions,
  root: string,
): boolean {
  return (
    patternsTargetGeneratedDirectory(options.paths, root, false) ||
    patternsTargetGeneratedDirectory(options.includePaths, root, false) ||
    patternsTargetGeneratedDirectory(options.globs, root, true) ||
    patternsTargetGeneratedDirectory(options.insensitiveGlobs, root, true)
  );
}

function patternsTargetGeneratedDirectory(
  patterns: readonly string[] | undefined,
  root: string,
  globs: boolean,
): boolean {
  return (patterns ?? []).some((pattern) => {
    const trimmed = pattern.trim();
    if (trimmed.length === 0 || (globs && trimmed.startsWith("!"))) {
      return false;
    }

    return patternSegmentsWithinRoot(trimmed, root).some((segment) =>
      globSegmentTargetsGeneratedDirectory(segment),
    );
  });
}

function patternSegmentsWithinRoot(pattern: string, root: string): string[] {
  const displayRoot = toDisplayPath(root).replace(/\/+$/, "");
  let normalized = pattern.split(sep).join("/").replace(/^\.\//, "");

  if (isAbsolute(pattern) && !containsGlobSyntax(pattern)) {
    const relativePattern = relative(root, resolve(pattern));
    normalized = toDisplayPath(relativePattern);
  } else if (
    normalized === displayRoot ||
    normalized.startsWith(`${displayRoot}/`)
  ) {
    normalized = normalized.slice(displayRoot.length).replace(/^\/+/, "");
  }

  return pathSegments(normalized);
}

function containsGlobSyntax(pattern: string): boolean {
  return /[*?[\]{}()]/.test(pattern);
}

function globSegmentTargetsGeneratedDirectory(segment: string): boolean {
  if (isGeneratedDirectoryName(segment)) {
    return true;
  }

  if (segment.startsWith("{") && segment.endsWith("}")) {
    return segment
      .slice(1, -1)
      .split(",")
      .some((alternative) => isGeneratedDirectoryName(alternative.trim()));
  }

  return false;
}
