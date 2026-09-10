import { realpath } from "node:fs/promises";
import { isAbsolute, join, relative, resolve, sep } from "node:path";
import { detail, EngineError, errorDetails } from "../engine/errors.js";
import { readWorkspaceManifest } from "../engine/manifest.js";
import { searchIntent } from "../engine/pipeline/search/intent.js";
import { runRgFileSearch, runRgSearch } from "../engine/service/lexical.js";
import { findNearestWorkspace } from "../engine/service/root.js";
import { enrichLexicalItemsWithStructure } from "../engine/service/structure-enrichment.js";
import type {
  ZvecGrepContextItem,
  ZvecGrepContextOptions,
  ZvecGrepContextResult,
} from "../engine/service/types.js";
import {
  matchesFileSelection,
  resolveFileTypePatterns,
} from "../engine/utils/file-selection.js";
import { toDisplayPath } from "../engine/utils/path.js";
import { TimingCollector } from "../engine/utils/timing.js";
import { addLiveKeywordMatches } from "./live-keywords.js";
import {
  livePathPriority,
  liveSearchOptions,
  rankLiveResults,
} from "./live-search.js";

export type CurrentSourceSearchInput = {
  /** Absolute request root; never use the daemon process's working directory. */
  root: string;
  query: string;
  options: ZvecGrepContextOptions;
  includeKeywords?: boolean;
  /** CLI path lookup is optional; API/MCP callers normally require text. */
  allowFileLookup?: boolean;
  signal?: AbortSignal;
};

export type CurrentSourceSearchResult =
  | { kind: "text"; result: ZvecGrepContextResult }
  | { kind: "files"; paths: string[]; truncated: boolean };

/**
 * Read current source without creating a model, index, service, or daemon job.
 * This is retrieval only: callers decide when incomplete lexical coverage is
 * appropriate and own all persistent-index preparation policy.
 */
export async function searchCurrentSource(
  input: CurrentSourceSearchInput,
): Promise<CurrentSourceSearchResult> {
  const { signal, includeKeywords = false, allowFileLookup = false } = input;
  signal?.throwIfAborted();
  if (!isAbsolute(input.root)) {
    throw new Error("Current-source search requires an absolute root.");
  }
  const query = input.query.trim();
  if (!query) {
    throw new EngineError("Current-source search requires a non-empty query", {
      code: "ZVEC_GREP.ENGINE.SERVICE.EMPTY_QUERY",
    });
  }
  // Capture caller-owned filters before file-type discovery or scope resolution
  // yields. Concurrent requests must not change this request's source scope.
  const options = snapshotOptions(input.options);
  const requestedRoot = resolve(input.root);
  const workspace = findNearestWorkspace(requestedRoot);
  const root = workspace?.root ?? requestedRoot;
  const manifest = workspace ? readWorkspaceManifest(workspace.home) : null;
  const types = await resolveFileTypePatterns(
    options.fileTypes,
    options.excludedFileTypes,
  );
  signal?.throwIfAborted();
  const requests = await Promise.all(
    (manifest?.rootPaths ?? [undefined]).map(async (scope) => {
      const canonicalScope = scope
        ? await realpath(scope.absolutePath).catch(() => scope.absolutePath)
        : root;
      signal?.throwIfAborted();
      return {
        displayPath: (path: string) => {
          if (!scope) return toDisplayPath(relative(root, path));
          const fromAlias = relative(scope.absolutePath, path);
          const tail =
            fromAlias === ".." ||
            fromAlias.startsWith(`..${sep}`) ||
            isAbsolute(fromAlias)
              ? relative(canonicalScope, path)
              : fromAlias;
          return toDisplayPath(relative(root, join(canonicalScope, tail)));
        },
        options: scope
          ? {
              ...options,
              rgPaths: [canonicalScope],
              includePaths: scope.include,
              excludePaths: scope.exclude,
              globs: scope.globs,
              insensitiveGlobs: scope.insensitiveGlobs,
              fileTypes: scope.fileTypes,
              excludedFileTypes: scope.excludedFileTypes,
              hidden: scope.hidden,
              noIgnore: scope.noIgnore,
              ignoreFiles: scope.ignoreFiles,
              maxDepth: scope.recursive ? scope.maxDepth : 1,
              maxFileSizeBytes: scope.maxFileSizeBytes,
              follow: scope.follow,
            }
          : options,
      };
    }),
  );
  signal?.throwIfAborted();
  const limit = options.limit ?? 10;
  if (allowFileLookup && searchIntent(query).kind === "path") {
    const paths = new Set<string>();
    let truncated = false;
    for (const request of requests) {
      signal?.throwIfAborted();
      const result = await runRgFileSearch({
        ...request.options,
        root,
        paths: request.options.rgPaths,
        limit,
        signal,
        rankPath: (path) => {
          const display = request.displayPath(path);
          return matchesFileSelection(display, options, types)
            ? livePathPriority(query, display, path)
            : 0;
        },
      });
      for (const path of result.paths) paths.add(request.displayPath(path));
      truncated ||= result.truncated;
    }
    signal?.throwIfAborted();
    if (paths.size) {
      const sorted = [...paths].sort(
        (a, b) =>
          livePathPriority(query, b, resolve(root, b)) -
            livePathPriority(query, a, resolve(root, a)) || a.localeCompare(b),
      );
      const selected = sorted.slice(0, limit);
      return {
        kind: "files",
        paths: selected,
        truncated: truncated || sorted.length > selected.length,
      };
    }
  }
  const results: ZvecGrepContextResult[] = [];
  const needsCallerSelection =
    manifest !== null &&
    !!(
      options.globs?.length ||
      options.insensitiveGlobs?.length ||
      options.fileTypes?.length ||
      options.excludedFileTypes?.length
    );
  for (const request of requests) {
    signal?.throwIfAborted();
    const result = await searchLiteral(
      root,
      query,
      liveSearchOptions(query, request.options),
      signal,
      needsCallerSelection
        ? (path) =>
            matchesFileSelection(request.displayPath(path), options, types)
        : undefined,
    );
    results.push({
      ...result,
      items: result.items.map((item) => ({
        ...item,
        file: {
          ...item.file,
          relativePath: request.displayPath(item.file.absolutePath),
        },
      })),
    });
  }
  const result: ZvecGrepContextResult = {
    ...results[0]!,
    coverage: results.some((item) => item.coverage === "rg_truncated")
      ? "rg_truncated"
      : "rg_exhaustive",
    items: results
      .flatMap((item) => item.items)
      .filter((item) =>
        matchesFileSelection(item.file.relativePath, options, types),
      ),
  };
  const ranked = rankLiveResults(result, limit);
  const current = includeKeywords
    ? await addLiveKeywordMatches(
        ranked,
        requests,
        (path) => matchesFileSelection(path, options, types),
        limit,
        signal,
      )
    : ranked;
  signal?.throwIfAborted();
  return { kind: "text", result: current };
}

async function searchLiteral(
  root: string,
  query: string,
  options: ZvecGrepContextOptions,
  signal: AbortSignal | undefined,
  acceptFile?: (absolutePath: string) => boolean,
): Promise<ZvecGrepContextResult> {
  const timings = new TimingCollector();
  const result = await timings.time("total", async () => {
    let rg;
    try {
      rg = await timings.time("rg_search", () =>
        runRgSearch({
          ...options,
          root,
          patterns: [query],
          paths: options.rgPaths,
          // Scope discovery remains native rg policy. Caller filters form an
          // intersection before accepted matches consume the existing cap.
          acceptFile,
          signal,
        }),
      );
    } catch (cause) {
      signal?.throwIfAborted();
      throw new EngineError("Search failed", {
        code: "ZVEC_GREP.ENGINE.SEARCH.FAILED",
        context: errorDetails([detail("source", "rg"), detail("root", root)]),
        cause,
      });
    }
    signal?.throwIfAborted();
    const structure = await timings.time("structure_enrichment", () =>
      enrichLexicalItemsWithStructure(
        root,
        rg.items,
        undefined,
        options.maxFileSizeBytes,
        false,
        { signal, preciseSourceOwnership: true },
      ),
    );
    signal?.throwIfAborted();
    const items = dedupeLiteralItems(structure.items);
    const noSearchableFiles =
      !!rg.diagnostics.missingPaths?.length &&
      rg.diagnostics.searchedPaths?.length === 0;
    return {
      query,
      root,
      source: "rg" as const,
      coverage: rg.diagnostics.truncated
        ? ("rg_truncated" as const)
        : ("rg_exhaustive" as const),
      items,
      diagnostics: {
        emptyReason: items.length
          ? undefined
          : noSearchableFiles
            ? ("no_searchable_files" as const)
            : ("no_matches" as const),
        rg: rg.diagnostics,
        structure: structure.diagnostics,
      },
    };
  });
  return {
    ...result,
    diagnostics: { ...result.diagnostics, timings: timings.entries() },
  };
}

function dedupeLiteralItems(
  items: readonly ZvecGrepContextItem[],
): ZvecGrepContextItem[] {
  const seen = new Set<string>();
  return items
    .filter((item) => {
      const key = item.entityId
        ? `entity:${item.entityId}`
        : ["range", item.file.absolutePath, JSON.stringify(item.range)].join(
            ":",
          );
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    })
    .map((item, index) => ({ ...item, rank: index + 1 }));
}

function snapshotOptions(
  options: ZvecGrepContextOptions,
): ZvecGrepContextOptions {
  return {
    ...options,
    queries: options.queries ? [...options.queries] : undefined,
    routes: options.routes?.map((route) => ({ ...route })),
    rgPaths: options.rgPaths ? [...options.rgPaths] : undefined,
    includePaths: options.includePaths ? [...options.includePaths] : undefined,
    excludePaths: options.excludePaths ? [...options.excludePaths] : undefined,
    globs: options.globs ? [...options.globs] : undefined,
    insensitiveGlobs: options.insensitiveGlobs
      ? [...options.insensitiveGlobs]
      : undefined,
    fileTypes: options.fileTypes ? [...options.fileTypes] : undefined,
    excludedFileTypes: options.excludedFileTypes
      ? [...options.excludedFileTypes]
      : undefined,
    ignoreFiles: options.ignoreFiles ? [...options.ignoreFiles] : undefined,
    symbolTypes: options.symbolTypes ? [...options.symbolTypes] : undefined,
    rgOptions: options.rgOptions
      ? {
          ...options.rgOptions,
          extraArgs: options.rgOptions.extraArgs
            ? [...options.rgOptions.extraArgs]
            : undefined,
          patternFiles: options.rgOptions.patternFiles
            ? [...options.rgOptions.patternFiles]
            : undefined,
        }
      : undefined,
  };
}
