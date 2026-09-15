import {
  closeSync,
  constants,
  existsSync,
  fstatSync,
  openSync,
  readFileSync,
  readSync,
  realpathSync,
  statSync,
} from "node:fs";
import { spawn, type ChildProcess } from "node:child_process";
import { isAbsolute, relative, resolve } from "node:path";
import { toDisplayPath } from "../utils/path.js";
import type {
  ZvecGrepContextItem,
  ZvecGrepSearchOptions,
  ZvecGrepRgDiagnostics,
} from "./types.js";

type RgSearchResult = {
  items: ZvecGrepContextItem[];
  diagnostics: ZvecGrepRgDiagnostics;
};

type RgSearchOptions = {
  root: string;
  patterns: readonly string[];
  paths?: readonly string[];
  limit?: number;
  includePaths?: readonly string[];
  excludePaths?: readonly string[];
  globs?: readonly string[];
  insensitiveGlobs?: readonly string[];
  fileTypes?: readonly string[];
  excludedFileTypes?: readonly string[];
  hidden?: boolean;
  noIgnore?: boolean;
  ignoreFiles?: readonly string[];
  maxDepth?: number;
  maxFileSizeBytes?: number;
  follow?: boolean;
  modifiedAfter?: number;
  modifiedBefore?: number;
  rgOptions?: ZvecGrepSearchOptions;
  /** Reject files before context reads and before accepted matches consume limit. */
  acceptFile?: (absolutePath: string) => boolean;
  /** Rank expanded matches; non-finite or non-positive scores are rejected. */
  rankItem?: (item: ZvecGrepContextItem) => number;
  /** Emit each matched anchor, up to 64 per match event; opt-in only. */
  matchAllOccurrences?: boolean;
  /** Maximum raw matches to process, including matches rejected by rankItem. */
  scanLimit?: number;
  /** Stop this rg process after the given elapsed time, returning partial results. */
  timeoutMs?: number;
  /** Cancel this invocation, draining its owned process before rejecting. */
  signal?: AbortSignal;
};

type CommandResult = {
  items: ZvecGrepContextItem[];
  truncated: boolean;
  args: string[];
};

type CommandOptions = Pick<
  RgSearchOptions,
  | "root"
  | "limit"
  | "modifiedAfter"
  | "modifiedBefore"
  | "rgOptions"
  | "acceptFile"
  | "rankItem"
  | "matchAllOccurrences"
  | "scanLimit"
  | "timeoutMs"
  | "signal"
> & {
  command: string;
  args: string[];
  parseLine(line: string, rank: number): ZvecGrepContextItem | null;
  parseMatches(line: string, rank: number): ParsedRipgrepMatchLine | null;
};

type ParsedRipgrepMatchLine = {
  items: Iterable<ZvecGrepContextItem, void>;
  truncated: boolean;
};

type ContextCacheBudget = {
  maxFiles: number;
  maxCharacters: number;
  maxFileBytes: number;
  characters: number;
  truncated: boolean;
  readBuffer?: Buffer;
};

type RipgrepRunOptions = RgSearchOptions;

type CheckedSearchPaths = {
  paths?: readonly string[];
  missingPaths: readonly string[];
};

type RipgrepBackend = {
  backend: "bundled-rg" | "rg";
  command: string;
};

const HARD_IGNORED_HIDDEN_DIRECTORIES = [".git", ".zvec-grep"] as const;
const MAX_BOUNDED_CONTEXT_FILE_BYTES = 1_048_576;
const MAX_BOUNDED_RG_JSON_CHARACTERS = 262_144;
const MAX_BOUNDED_STDERR_CHARACTERS = 16_384;
const MAX_BOUNDED_SUBMATCHES = 64;

let bundledRipgrepPath: string | null | undefined;

export async function runRgSearch(
  options: RgSearchOptions,
): Promise<RgSearchResult> {
  options.signal?.throwIfAborted();
  options = {
    ...snapshotSelection(options),
    patterns: [...options.patterns],
    rgOptions: options.rgOptions
      ? {
          ...options.rgOptions,
          extraArgs: options.rgOptions.extraArgs?.slice(),
          patternFiles: options.rgOptions.patternFiles?.slice(),
        }
      : undefined,
  };
  const backends = await ripgrepBackends();
  options.signal?.throwIfAborted();
  const paths = checkSearchPaths(options.root, options.paths);
  if (options.paths && options.paths.length > 0 && !paths.paths?.length) {
    const backend = backends[0]!;
    const args = buildRipgrepArgs({
      ...options,
    });

    return {
      items: [],
      diagnostics: {
        backend: backend.backend,
        command: backend.command,
        args,
        ignoredDirectories: HARD_IGNORED_HIDDEN_DIRECTORIES,
        missingPaths: paths.missingPaths,
        searchedPaths: [],
        limit: options.limit,
        truncated: false,
      },
    };
  }

  let commandMissing: unknown;
  for (const backend of backends) {
    options.signal?.throwIfAborted();
    try {
      const result = await runRipgrep(
        {
          ...options,
          paths: paths.paths,
        },
        backend.command,
      );
      options.signal?.throwIfAborted();

      return {
        items: result.items,
        diagnostics: {
          backend: backend.backend,
          command: backend.command,
          args: result.args,
          ignoredDirectories: HARD_IGNORED_HIDDEN_DIRECTORIES,
          missingPaths:
            paths.missingPaths.length > 0 ? paths.missingPaths : undefined,
          searchedPaths: paths.paths,
          limit: options.limit,
          truncated: result.truncated,
        },
      };
    } catch (error) {
      options.signal?.throwIfAborted();
      if (!isCommandMissing(error)) {
        throw error;
      }
      commandMissing = error;
    }
  }

  throw commandMissing instanceof Error
    ? commandMissing
    : new Error("ripgrep command not found");
}

function snapshotSelection<T extends Omit<RgSearchOptions, "patterns">>(
  options: T,
): T {
  return {
    ...options,
    paths: options.paths?.slice(),
    includePaths: options.includePaths?.slice(),
    excludePaths: options.excludePaths?.slice(),
    globs: options.globs?.slice(),
    insensitiveGlobs: options.insensitiveGlobs?.slice(),
    fileTypes: options.fileTypes?.slice(),
    excludedFileTypes: options.excludedFileTypes?.slice(),
    ignoreFiles: options.ignoreFiles?.slice(),
  };
}

/** Abort belongs to this child only; callers settle after its close event. */
function ownCommandCancellation(
  child: ChildProcess,
  signal?: AbortSignal,
  onAbort?: () => void,
): () => void {
  const abort = () => {
    onAbort?.();
    // No grace-period work is useful after cancellation. Killing this owned
    // process also avoids waiting forever for a child that ignores SIGTERM.
    child.kill("SIGKILL");
  };
  if (signal?.aborted) abort();
  else signal?.addEventListener("abort", abort, { once: true });
  return () => signal?.removeEventListener("abort", abort);
}

function rejectUnchanged(
  reject: (reason?: unknown) => void,
  reason: unknown,
): void {
  // AbortSignal.reason may be any value, including a non-Error sentinel.
  reject(reason);
}

async function ripgrepBackends(): Promise<RipgrepBackend[]> {
  const bundled = await resolveBundledRipgrepPath();
  const backends: RipgrepBackend[] = [];

  if (bundled) {
    backends.push({
      backend: "bundled-rg",
      command: bundled,
    });
  }

  backends.push({
    backend: "rg",
    command: "rg",
  });

  return backends;
}

async function resolveBundledRipgrepPath(): Promise<string | undefined> {
  if (bundledRipgrepPath !== undefined) {
    return bundledRipgrepPath ?? undefined;
  }

  try {
    const { rgPath } = await import("@vscode/ripgrep");
    bundledRipgrepPath = rgPath;
  } catch {
    bundledRipgrepPath = null;
  }

  return bundledRipgrepPath ?? undefined;
}

function runRipgrep(
  options: RipgrepRunOptions,
  command: string,
): Promise<CommandResult> {
  const args = buildRipgrepArgs(options);

  return runCommand({
    command,
    args,
    root: options.root,
    limit: options.limit,
    modifiedAfter: options.modifiedAfter,
    modifiedBefore: options.modifiedBefore,
    rgOptions: options.rgOptions,
    acceptFile: options.acceptFile,
    rankItem: options.rankItem,
    matchAllOccurrences: options.matchAllOccurrences,
    scanLimit: options.scanLimit,
    timeoutMs: options.timeoutMs,
    signal: options.signal,
    parseLine: (line, rank) => parseRipgrepJsonLine(line, options.root, rank),
    parseMatches: (line, rank) =>
      parseRipgrepJsonMatches(line, options.root, rank, true),
  });
}

function buildRipgrepArgs(options: RipgrepRunOptions): string[] {
  return [
    "--json",
    "--line-number",
    "--column",
    "--with-filename",
    "--color",
    "never",
    ...ripgrepSearchArgs(options.rgOptions),
    ...ripgrepSelectionArgs(options),
    ...(options.rgOptions?.extraArgs ?? []),
    ...patternArgs(options.patterns, options.rgOptions?.patternFiles),
    "--",
    ...(options.paths && options.paths.length > 0
      ? options.paths
      : [options.root]),
  ];
}

function ripgrepSelectionArgs(
  options: Omit<RgSearchOptions, "patterns">,
): string[] {
  return [
    ...hiddenSearchArgs(
      options.includePaths,
      options.hidden,
      options.rgOptions,
    ),
    ...ripgrepDiscoveryArgs(options),
    ...pathFilterArgs(options.includePaths, false),
    ...pathFilterArgs(options.excludePaths, true),
    ...globFilterArgs(options.globs, "--glob"),
    ...globFilterArgs(options.insensitiveGlobs, "--iglob"),
    ...(options.fileTypes ?? []).flatMap((type) => ["--type", type]),
    ...(options.excludedFileTypes ?? []).flatMap((type) => [
      "--type-not",
      type,
    ]),
    ...hardIgnoredHiddenDirectoryArgs(),
  ];
}

/** Discover file names without reading contents or opening an embedding index. */
export async function runRgFileSearch(
  options: Omit<RgSearchOptions, "patterns" | "rgOptions"> & {
    rankPath(absolutePath: string): number;
  },
): Promise<{ paths: string[]; truncated: boolean }> {
  options.signal?.throwIfAborted();
  options = snapshotSelection(options);
  const checked = checkSearchPaths(options.root, options.paths);
  if (options.paths?.length && !checked.paths?.length)
    return { paths: [], truncated: false };
  // Node/rg canonicalize cwd, but configured roots may retain a directory
  // alias (e.g. /var vs /private/var). Use cwd-relative inputs so anchored
  // globs refer to the workspace, not to an absolute alias prefix. Only the
  // supplied roots are resolved; --follow still controls child symlinks.
  const canonicalRoot = realpathSync(options.root);
  const searchPaths = (
    checked.paths?.length ? checked.paths : [options.root]
  ).map(
    (path) =>
      relative(
        canonicalRoot,
        realpathSync(resolveSearchPath(options.root, path)),
      ) || ".",
  );
  const args = [
    "--files",
    "--null",
    ...ripgrepSelectionArgs(options),
    "--",
    ...searchPaths,
  ];
  const limit = options.limit ?? 200;
  let missing: unknown;
  for (const backend of await ripgrepBackends()) {
    options.signal?.throwIfAborted();
    try {
      const result = await new Promise<{ paths: string[]; truncated: boolean }>(
        (resolvePromise, reject) => {
          const child = spawn(backend.command, args, {
            cwd: options.root,
            stdio: ["ignore", "pipe", "pipe"],
          });
          const selected: { path: string; rank: number }[] = [];
          const mtimeCache = new Map<string, boolean>();
          let pending = "";
          let stderr = "";
          let matches = 0;
          let failure: { error: unknown } | undefined;
          const disposeCancellation = ownCommandCancellation(
            child,
            options.signal,
          );
          child.stdout.setEncoding("utf8");
          child.stderr.setEncoding("utf8");
          child.stdout.on("data", (chunk: string) => {
            if (options.signal?.aborted || failure !== undefined) return;
            try {
              pending += chunk;
              let boundary: number;
              while ((boundary = pending.indexOf("\0")) >= 0) {
                const path = resolveSearchPath(
                  options.root,
                  pending.slice(0, boundary),
                );
                pending = pending.slice(boundary + 1);
                const rank = options.rankPath(path);
                options.signal?.throwIfAborted();
                if (
                  rank <= 0 ||
                  !matchesModifiedTime(path, options, mtimeCache)
                )
                  continue;
                matches++;
                selected.push({ path, rank });
                selected.sort(
                  (a, b) => b.rank - a.rank || a.path.localeCompare(b.path),
                );
                if (selected.length > limit) selected.pop();
              }
            } catch (error) {
              failure = { error };
              child.kill();
            }
          });
          child.stderr.on("data", (chunk: string) => {
            stderr += chunk;
          });
          child.on("error", (error) => {
            failure = { error };
          });
          child.on("close", (code) => {
            disposeCancellation();
            if (options.signal?.aborted) {
              rejectUnchanged(reject, options.signal.reason);
              return;
            }
            if (failure !== undefined) {
              rejectUnchanged(reject, failure.error);
              return;
            }
            if (code === 0 || code === 1) {
              resolvePromise({
                paths: selected.map((item) => item.path),
                truncated: matches > selected.length,
              });
            } else {
              reject(
                new Error(
                  `${backend.command} failed with exit code ${code}: ${stderr.trim()}`,
                ),
              );
            }
          });
        },
      );
      options.signal?.throwIfAborted();
      return result;
    } catch (error) {
      options.signal?.throwIfAborted();
      if (!isCommandMissing(error)) throw error;
      missing = error;
    }
  }
  throw missing instanceof Error
    ? missing
    : new Error("ripgrep command not found");
}

function ripgrepSearchArgs(
  options: ZvecGrepSearchOptions | undefined,
): string[] {
  const args: string[] = [];
  if (options?.fixedStrings) {
    args.push("--fixed-strings");
  }
  if (options?.ignoreCase) {
    args.push("--ignore-case");
  }
  if (options?.wordRegexp) {
    args.push("--word-regexp");
  }
  return args;
}

function hiddenSearchArgs(
  includePaths: readonly string[] | undefined,
  hidden: boolean | undefined,
  options: ZvecGrepSearchOptions | undefined,
): string[] {
  return hidden || options?.hidden || includesHiddenPath(includePaths)
    ? ["--hidden"]
    : [];
}

function ripgrepDiscoveryArgs(
  options: Omit<RgSearchOptions, "patterns">,
): string[] {
  return [
    ...(options.noIgnore ? ["--no-ignore"] : []),
    ...(options.ignoreFiles ?? []).flatMap((path) => ["--ignore-file", path]),
    ...(options.maxDepth !== undefined
      ? ["--max-depth", String(options.maxDepth)]
      : []),
    ...(options.maxFileSizeBytes !== undefined
      ? ["--max-filesize", String(options.maxFileSizeBytes)]
      : []),
    ...(options.follow ? ["--follow"] : []),
  ];
}

function globFilterArgs(
  patterns: readonly string[] | undefined,
  option: "--glob" | "--iglob",
): string[] {
  return (patterns ?? []).flatMap((pattern) => [option, pattern]);
}

function hardIgnoredHiddenDirectoryArgs(): string[] {
  return HARD_IGNORED_HIDDEN_DIRECTORIES.flatMap((directory) => [
    "--glob",
    `!**/${directory}/**`,
  ]);
}

function pathFilterArgs(
  patterns: readonly string[] | undefined,
  negated: boolean,
): string[] {
  if (!patterns || patterns.length === 0) {
    return [];
  }

  return patterns.flatMap((pattern) =>
    expandRipgrepPathGlob(pattern).flatMap((expandedPattern) => [
      "--glob",
      negated ? `!${expandedPattern}` : expandedPattern,
    ]),
  );
}

function expandRipgrepPathGlob(pattern: string): string[] {
  const normalized = pattern.startsWith("./") ? pattern.slice(2) : pattern;
  if (normalized.startsWith("**/") || isAbsolute(normalized)) {
    return [normalized];
  }

  return [normalized, `**/${normalized}`];
}

function patternArgs(
  patterns: readonly string[],
  patternFiles: readonly string[] | undefined,
): string[] {
  return [
    ...patterns.flatMap((pattern) => ["--regexp", pattern]),
    ...(patternFiles ?? []).flatMap((path) => ["--file", path]),
  ];
}

function includesHiddenPath(patterns: readonly string[] | undefined): boolean {
  if (!patterns || patterns.length === 0) {
    return false;
  }

  return patterns.some((pattern) =>
    pattern.split(/[\\/]+/).some((segment) => isHiddenPatternSegment(segment)),
  );
}

function isHiddenPatternSegment(segment: string): boolean {
  return segment.startsWith(".") && segment !== "." && segment !== "..";
}

function checkSearchPaths(
  root: string,
  paths: readonly string[] | undefined,
): CheckedSearchPaths {
  if (!paths || paths.length === 0) {
    return {
      paths,
      missingPaths: [],
    };
  }

  const existing: string[] = [];
  const missing: string[] = [];
  for (const path of paths) {
    if (existsSync(resolveSearchPath(root, path))) {
      existing.push(path);
    } else {
      missing.push(path);
    }
  }

  return {
    paths: existing,
    missingPaths: missing,
  };
}

function resolveSearchPath(root: string, path: string): string {
  return isAbsolute(path) ? path : resolve(root, path);
}

function cachedFileAcceptance(
  acceptFile: RgSearchOptions["acceptFile"],
): (path: string) => boolean {
  const cache = new Map<string, boolean>();
  return (path) => {
    if (!acceptFile) return true;
    const cached = cache.get(path);
    if (cached !== undefined) return cached;
    const accepted = acceptFile(path);
    if (cache.size >= 256) cache.clear();
    cache.set(path, accepted);
    return accepted;
  };
}

function runCommand(options: CommandOptions): Promise<CommandResult> {
  options.signal?.throwIfAborted();
  if (
    options.rankItem !== undefined ||
    options.matchAllOccurrences === true ||
    options.scanLimit !== undefined ||
    options.timeoutMs !== undefined
  ) {
    return runBoundedCommand(options);
  }

  return new Promise((resolvePromise, reject) => {
    const child = spawn(options.command, options.args, {
      cwd: options.root,
      stdio: ["ignore", "pipe", "pipe"],
    });
    const items: ZvecGrepContextItem[] = [];
    let stdoutBuffer = "";
    let stderr = "";
    let truncated = false;
    let killedAfterLimit = false;
    let failure: { error: unknown } | undefined;
    const disposeCancellation = ownCommandCancellation(child, options.signal);
    const hasLimit = options.limit !== undefined;
    const mtimeCache = new Map<string, boolean>();
    const contextCache = new Map<string, string[] | null>();
    const acceptsFile = cachedFileAcceptance(options.acceptFile);
    const collect = (line: string) => {
      const parsedItem = options.parseLine(line, items.length + 1);
      if (!parsedItem) return;
      const accepted = acceptsFile(parsedItem.file.absolutePath);
      options.signal?.throwIfAborted();
      if (!accepted) return;
      const item = expandContextItem(
        parsedItem,
        options.rgOptions,
        contextCache,
      );
      if (matchesModifiedTime(item.file.absolutePath, options, mtimeCache)) {
        items.push(item);
      }
    };
    const fail = (error: unknown) => {
      failure ??= { error };
      stdoutBuffer = "";
      child.kill();
    };

    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");

    child.stdout.on("data", (chunk: string) => {
      if (options.signal?.aborted || failure) return;
      stdoutBuffer += chunk;
      let newlineIndex = stdoutBuffer.indexOf("\n");

      try {
        while (newlineIndex >= 0) {
          const line = stdoutBuffer.slice(0, newlineIndex);
          stdoutBuffer = stdoutBuffer.slice(newlineIndex + 1);
          newlineIndex = stdoutBuffer.indexOf("\n");
          collect(line);

          if (hasLimit && items.length > options.limit!) {
            truncated = true;
            killedAfterLimit = true;
            child.kill();
            break;
          }
        }
      } catch (error) {
        fail(error);
      }
    });

    child.stderr.on("data", (chunk: string) => {
      stderr += chunk;
    });

    child.on("error", fail);

    child.on("close", (code) => {
      disposeCancellation();
      if (options.signal?.aborted) {
        rejectUnchanged(reject, options.signal.reason);
        return;
      }
      if (
        !failure &&
        !killedAfterLimit &&
        stdoutBuffer.length > 0 &&
        (!hasLimit || items.length < options.limit!)
      ) {
        try {
          collect(stdoutBuffer);
        } catch (error) {
          fail(error);
        }
      }
      if (failure !== undefined) {
        rejectUnchanged(reject, failure.error);
        return;
      }

      if (code === 0 || code === 1 || killedAfterLimit) {
        resolvePromise({
          items: hasLimit ? items.slice(0, options.limit) : items,
          truncated,
          args: options.args,
        });
        return;
      }

      reject(
        new Error(
          `${options.command} failed with exit code ${code}: ${stderr.trim()}`,
        ),
      );
    });
  });
}

/** Keep the original literal-search path above unchanged unless opted in. */
function runBoundedCommand(options: CommandOptions): Promise<CommandResult> {
  return new Promise((resolvePromise, reject) => {
    options.signal?.throwIfAborted();
    if (
      options.scanLimit !== undefined &&
      (!Number.isSafeInteger(options.scanLimit) || options.scanLimit < 0)
    ) {
      reject(new RangeError("scanLimit must be a non-negative safe integer"));
      return;
    }
    if (
      options.timeoutMs !== undefined &&
      (!Number.isFinite(options.timeoutMs) || options.timeoutMs < 0)
    ) {
      reject(new RangeError("timeoutMs must be a non-negative finite number"));
      return;
    }

    const child = spawn(options.command, options.args, {
      cwd: options.root,
      stdio: ["ignore", "pipe", "pipe"],
    });
    const selected: {
      item: ZvecGrepContextItem;
      score: number;
      sequence: number;
    }[] = [];
    const contextCache = new Map<string, string[] | null>();
    const contextBudget: ContextCacheBudget = {
      maxFiles: 8,
      maxCharacters: 1_048_576,
      maxFileBytes: MAX_BOUNDED_CONTEXT_FILE_BYTES,
      characters: 0,
      truncated: false,
    };
    const mtimeCache = new Map<string, boolean>();
    const acceptsFile = cachedFileAcceptance(options.acceptFile);
    const deadline =
      options.timeoutMs === undefined
        ? Infinity
        : performance.now() + options.timeoutMs;
    let stdoutBuffer = "";
    let stderr = "";
    let stderrTruncated = false;
    let matches = 0;
    let anchors = 0;
    let truncated = false;
    let stopped = false;
    let settled = false;
    let failure: { error: unknown } | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const disposeCancellation = ownCommandCancellation(
      child,
      options.signal,
      () => {
        stopped = true;
        stdoutBuffer = "";
        clearTimeout(timer);
      },
    );

    const stop = () => {
      truncated = true;
      stopped = true;
      stdoutBuffer = "";
      child.kill();
    };
    const fail = (error: unknown) => {
      if (settled) return;
      failure ??= { error };
      stopped = true;
      clearTimeout(timer);
      child.kill();
    };
    const processAnchor = (parsedItem: ZvecGrepContextItem) => {
      anchors++;
      if (!acceptsFile(parsedItem.file.absolutePath)) return;
      options.signal?.throwIfAborted();
      const item = expandContextItem(
        parsedItem,
        options.rgOptions,
        contextCache,
        contextBudget,
      );
      if (contextBudget.truncated) truncated = true;
      if (mtimeCache.size >= 256) mtimeCache.clear();
      if (!matchesModifiedTime(item.file.absolutePath, options, mtimeCache))
        return;
      const score = options.rankItem ? options.rankItem(item) : 1;
      options.signal?.throwIfAborted();
      if (Number.isFinite(score) && score > 0) {
        selected.push({ item, score, sequence: anchors });
        if (options.rankItem) {
          selected.sort((a, b) => b.score - a.score || a.sequence - b.sequence);
          if (options.limit !== undefined && selected.length > options.limit) {
            selected.pop();
            truncated = true;
          }
        } else if (
          options.limit !== undefined &&
          selected.length > options.limit
        ) {
          stop();
        }
      }
    };
    const processLine = (line: string) => {
      if (stopped) return;
      // A large stdout chunk or synchronous ranker can delay timer callbacks.
      if (performance.now() >= deadline) {
        stop();
        return;
      }
      const rank = options.rankItem ? anchors + 1 : selected.length + 1;
      let parsed: ParsedRipgrepMatchLine | null;
      if (options.matchAllOccurrences === true) {
        parsed = options.parseMatches(line, rank);
      } else {
        const item = options.parseLine(line, rank);
        parsed = item ? { items: [item], truncated: false } : null;
      }
      if (!parsed) return;
      matches++;
      if (options.scanLimit !== undefined && matches > options.scanLimit) {
        stop();
        return;
      }
      if (parsed.truncated) truncated = true;
      const iterator = parsed.items[Symbol.iterator]();
      while (!stopped) {
        // Position conversion is lazy so the deadline also bounds work on
        // later anchors inside one JSON event, not just between match lines.
        if (performance.now() >= deadline) {
          stop();
          return;
        }
        const next = iterator.next();
        if (next.done) return;
        processAnchor(next.value);
        if (!stopped && performance.now() >= deadline) stop();
      }
    };

    if (options.timeoutMs !== undefined && !options.signal?.aborted) {
      timer = setTimeout(stop, options.timeoutMs);
      timer.unref();
    }
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      if (stopped) return;
      try {
        let offset = 0;
        while (!stopped && offset < chunk.length) {
          const newlineIndex = chunk.indexOf("\n", offset);
          const end = newlineIndex < 0 ? chunk.length : newlineIndex;
          if (
            stdoutBuffer.length + end - offset >
            MAX_BOUNDED_RG_JSON_CHARACTERS
          ) {
            stop();
            return;
          }
          stdoutBuffer += chunk.slice(offset, end);
          if (newlineIndex < 0) return;
          const line = stdoutBuffer;
          stdoutBuffer = "";
          processLine(line);
          offset = newlineIndex + 1;
        }
      } catch (error) {
        fail(error);
      }
    });
    child.stderr.on("data", (chunk: string) => {
      const remaining = MAX_BOUNDED_STDERR_CHARACTERS - stderr.length;
      stderr += chunk.slice(0, remaining);
      if (chunk.length > remaining) stderrTruncated = true;
    });
    child.on("error", fail);
    child.on("close", (code, signal) => {
      clearTimeout(timer);
      disposeCancellation();
      if (settled) return;
      try {
        if (!stopped && stdoutBuffer.length > 0) processLine(stdoutBuffer);
      } catch (error) {
        fail(error);
      }
      settled = true;
      if (options.signal?.aborted) {
        rejectUnchanged(reject, options.signal.reason);
        return;
      }
      if (failure) {
        const error = failure.error;
        reject(error instanceof Error ? error : new Error(String(error)));
        return;
      }
      if (
        code === 0 ||
        code === 1 ||
        (stopped && code === null && signal === "SIGTERM")
      ) {
        const items = selected.map(({ item }, index) =>
          options.rankItem ? { ...item, rank: index + 1 } : item,
        );
        resolvePromise({
          items:
            options.limit === undefined ? items : items.slice(0, options.limit),
          truncated,
          args: options.args,
        });
      } else {
        reject(
          new Error(
            `${options.command} failed with exit code ${code}: ${stderr.trim()}${stderrTruncated ? "\n[stderr truncated]" : ""}`,
          ),
        );
      }
    });
  });
}

function expandContextItem(
  item: ZvecGrepContextItem,
  options: ZvecGrepSearchOptions | undefined,
  cache: Map<string, string[] | null>,
  budget?: ContextCacheBudget,
): ZvecGrepContextItem {
  if (item.range.kind !== "text") {
    return item;
  }

  const before = options?.beforeContext ?? 0;
  const after = options?.afterContext ?? 0;
  if (before === 0 && after === 0) {
    return item;
  }

  const lines = readTextLines(item.file.absolutePath, cache, budget);
  if (!lines || lines.length === 0) {
    return item;
  }

  const startLine = Math.max(1, item.range.startLine - before);
  const endLine = Math.min(lines.length, item.range.endLine + after);
  const content = lines.slice(startLine - 1, endLine).join("\n");

  return {
    ...item,
    range: {
      kind: "text",
      startLine,
      endLine,
      startOffset: 0,
      endOffset: lines[endLine - 1]?.length ?? 0,
    },
    excerptRange: item.range,
    content,
  };
}

function readTextLines(
  path: string,
  cache: Map<string, string[] | null>,
  budget?: ContextCacheBudget,
): string[] | null {
  if (cache.has(path)) {
    return cache.get(path) ?? null;
  }

  let lines: string[] | null = null;
  try {
    const text = budget
      ? readBoundedContextText(path, budget)
      : readFileSync(path, "utf8");
    lines = text === null ? null : text.split(/\r?\n/);
    if (lines?.at(-1) === "") {
      lines = lines.slice(0, -1);
    }
  } catch {
    lines = null;
  }

  if (budget) {
    const characters = textLineCharacters(lines);
    if (characters > budget.maxCharacters) {
      budget.truncated = true;
      lines = null;
    }
    const cachedCharacters = lines === null ? 0 : characters;
    while (
      cache.size >= budget.maxFiles ||
      budget.characters + cachedCharacters > budget.maxCharacters
    ) {
      const oldest = cache.keys().next().value;
      if (oldest === undefined) break;
      budget.characters -= textLineCharacters(cache.get(oldest) ?? null);
      cache.delete(oldest);
    }
    budget.characters += cachedCharacters;
  }
  cache.set(path, lines);
  return lines;
}

function readBoundedContextText(
  path: string,
  budget: ContextCacheBudget,
): string | null {
  // Non-blocking open also avoids hanging if a searched file becomes a FIFO.
  const descriptor = openSync(path, constants.O_RDONLY | constants.O_NONBLOCK);
  try {
    const info = fstatSync(descriptor);
    if (!info.isFile() || info.size > budget.maxFileBytes) {
      budget.truncated = true;
      return null;
    }
    // Reuse one fixed buffer per search. fstat alone cannot bound a file that
    // grows while being read; one extra byte detects that overflow as well.
    const buffer = (budget.readBuffer ??= Buffer.allocUnsafe(
      budget.maxFileBytes + 1,
    ));
    let length = 0;
    while (length < buffer.length) {
      const count = readSync(
        descriptor,
        buffer,
        length,
        buffer.length - length,
        null,
      );
      if (count === 0) break;
      length += count;
    }
    if (length > budget.maxFileBytes) {
      budget.truncated = true;
      return null;
    }
    return buffer.toString("utf8", 0, length);
  } finally {
    closeSync(descriptor);
  }
}

function textLineCharacters(lines: string[] | null): number {
  return lines?.reduce((total, line) => total + line.length + 1, 0) ?? 0;
}

function matchesModifiedTime(
  path: string,
  options: {
    modifiedAfter?: number;
    modifiedBefore?: number;
  },
  cache: Map<string, boolean>,
): boolean {
  if (
    options.modifiedAfter === undefined &&
    options.modifiedBefore === undefined
  ) {
    return true;
  }

  const cached = cache.get(path);
  if (cached !== undefined) {
    return cached;
  }

  let matched = false;
  try {
    const info = statSync(path, { throwIfNoEntry: false });
    if (info?.isFile()) {
      const modifiedTime = info.mtimeMs;
      matched =
        (options.modifiedAfter === undefined ||
          modifiedTime >= options.modifiedAfter) &&
        (options.modifiedBefore === undefined ||
          modifiedTime <= options.modifiedBefore);
    }
  } catch {
    matched = false;
  }

  cache.set(path, matched);
  return matched;
}

function parseRipgrepJsonLine(
  line: string,
  root: string,
  rank: number,
): ZvecGrepContextItem | null {
  const first = parseRipgrepJsonMatches(line, root, rank, false)
    ?.items[Symbol.iterator]()
    .next();
  return first && !first.done ? first.value : null;
}

function parseRipgrepJsonMatches(
  line: string,
  root: string,
  rank: number,
  matchAllOccurrences: boolean,
): ParsedRipgrepMatchLine | null {
  if (line.trim().length === 0) {
    return null;
  }

  let event: unknown;
  try {
    event = JSON.parse(line);
  } catch {
    return null;
  }

  if (!isRecord(event) || event.type !== "match" || !isRecord(event.data)) {
    return null;
  }

  const data = event.data;
  if (!isRecord(data.path) || typeof data.path.text !== "string") {
    return null;
  }

  if (!isRecord(data.lines) || typeof data.lines.text !== "string") {
    return null;
  }

  if (typeof data.line_number !== "number") {
    return null;
  }

  const path = normalizeResultPath(root, data.path.text);
  const lineText = trimTrailingNewline(data.lines.text);
  const lineNumber = data.line_number;
  const submatches = Array.isArray(data.submatches) ? data.submatches : [];
  const selectedSubmatches =
    matchAllOccurrences && submatches.length > 0
      ? submatches.slice(0, MAX_BOUNDED_SUBMATCHES)
      : [submatches[0]];
  return {
    truncated:
      matchAllOccurrences && submatches.length > MAX_BOUNDED_SUBMATCHES,
    items: (function* (): Generator<ZvecGrepContextItem, void> {
      for (const [index, value] of selectedSubmatches.entries()) {
        const submatch = isRecord(value) ? value : undefined;
        const start = textPositionAtByteOffset(
          lineText,
          typeof submatch?.start === "number" ? submatch.start : 0,
        );
        const end = textPositionAtByteOffset(
          lineText,
          typeof submatch?.end === "number"
            ? submatch.end
            : Buffer.byteLength(lineText, "utf8"),
        );
        yield {
          kind: "lexical_match",
          rank: rank + index,
          file: path,
          range: {
            kind: "text",
            startLine: lineNumber + start.lineOffset,
            endLine: lineNumber + end.lineOffset,
            startOffset: start.column,
            endOffset: end.column,
          },
          content: lineText,
          status: "fresh",
          matchedBy: "lexical",
        };
      }
    })(),
  };
}

function normalizeResultPath(root: string, path: string) {
  const absolutePath = isAbsolute(path) ? resolve(path) : resolve(root, path);

  return {
    absolutePath,
    relativePath: toDisplayPath(relative(root, absolutePath) || "."),
    rootPath: root,
  };
}

function trimTrailingNewline(value: string): string {
  return value.replace(/\r?\n$/, "");
}

function textPositionAtByteOffset(
  value: string,
  byteOffset: number,
): { lineOffset: number; column: number } {
  const prefix = Buffer.from(value, "utf8")
    .subarray(0, Math.max(0, byteOffset))
    .toString("utf8");
  const lines = prefix.split("\n");
  return {
    lineOffset: lines.length - 1,
    column: lines.at(-1)?.replace(/\r$/, "").length ?? 0,
  };
}

function isCommandMissing(error: unknown): boolean {
  return isRecord(error) && error.code === "ENOENT";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
