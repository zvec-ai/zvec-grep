import { existsSync, readFileSync, statSync } from "node:fs";
import { spawn } from "node:child_process";
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
  scanLimit?: number;
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
};

type CommandResult = {
  items: ZvecGrepContextItem[];
  truncated: boolean;
  args: string[];
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

let bundledRipgrepPath: string | null | undefined;

export async function runRgSearch(
  options: RgSearchOptions,
): Promise<RgSearchResult> {
  const backends = await ripgrepBackends();
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
        scanLimit: options.scanLimit,
        truncated: false,
      },
    };
  }

  let commandMissing: unknown;
  for (const backend of backends) {
    try {
      const result = await runRipgrep(
        {
          ...options,
          paths: paths.paths,
        },
        backend.command,
      );

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
          scanLimit: options.scanLimit,
          truncated: result.truncated,
        },
      };
    } catch (error) {
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
    limit: options.scanLimit ?? options.limit,
    modifiedAfter: options.modifiedAfter,
    modifiedBefore: options.modifiedBefore,
    rgOptions: options.rgOptions,
    parseLine: (line, rank) => parseRipgrepJsonLine(line, options.root, rank),
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
    ...(options.rgOptions?.extraArgs ?? []),
    ...patternArgs(options.patterns, options.rgOptions?.patternFiles),
    "--",
    ...(options.paths && options.paths.length > 0
      ? options.paths
      : [options.root]),
  ];
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

function ripgrepDiscoveryArgs(options: RipgrepRunOptions): string[] {
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

function runCommand(options: {
  command: string;
  args: string[];
  root: string;
  limit?: number;
  modifiedAfter?: number;
  modifiedBefore?: number;
  rgOptions?: ZvecGrepSearchOptions;
  parseLine(line: string, rank: number): readonly ZvecGrepContextItem[];
}): Promise<CommandResult> {
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
    const hasLimit = options.limit !== undefined;
    const mtimeCache = new Map<string, boolean>();
    const contextCache = new Map<string, string[] | null>();
    const appendParsedItems = (line: string): boolean => {
      const parsedItems = options.parseLine(line, items.length + 1);
      for (const parsedItem of parsedItems) {
        const item = expandContextItem(
          parsedItem,
          options.rgOptions,
          contextCache,
        );
        if (matchesModifiedTime(item.file.absolutePath, options, mtimeCache)) {
          items.push(item);
        }

        if (hasLimit && items.length > options.limit!) {
          truncated = true;
          killedAfterLimit = true;
          child.kill();
          return true;
        }
      }
      return false;
    };

    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");

    child.stdout.on("data", (chunk: string) => {
      stdoutBuffer += chunk;
      let newlineIndex = stdoutBuffer.indexOf("\n");

      while (newlineIndex >= 0) {
        const line = stdoutBuffer.slice(0, newlineIndex);
        stdoutBuffer = stdoutBuffer.slice(newlineIndex + 1);
        newlineIndex = stdoutBuffer.indexOf("\n");

        if (appendParsedItems(line)) {
          break;
        }
      }
    });

    child.stderr.on("data", (chunk: string) => {
      stderr += chunk;
    });

    child.on("error", (error) => {
      reject(error);
    });

    child.on("close", (code) => {
      if (
        !killedAfterLimit &&
        stdoutBuffer.length > 0 &&
        (!hasLimit || items.length < options.limit!)
      ) {
        appendParsedItems(stdoutBuffer);
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

function expandContextItem(
  item: ZvecGrepContextItem,
  options: ZvecGrepSearchOptions | undefined,
  cache: Map<string, string[] | null>,
): ZvecGrepContextItem {
  if (item.range.kind !== "text") {
    return item;
  }

  const before = options?.beforeContext ?? 0;
  const after = options?.afterContext ?? 0;
  if (before === 0 && after === 0) {
    return item;
  }

  const lines = readTextLines(item.file.absolutePath, cache);
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
): string[] | null {
  if (cache.has(path)) {
    return cache.get(path) ?? null;
  }

  let lines: string[] | null = null;
  try {
    lines = readFileSync(path, "utf8").split(/\r?\n/);
    if (lines.at(-1) === "") {
      lines = lines.slice(0, -1);
    }
  } catch {
    lines = null;
  }

  cache.set(path, lines);
  return lines;
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
): ZvecGrepContextItem[] {
  if (line.trim().length === 0) {
    return [];
  }

  let event: unknown;
  try {
    event = JSON.parse(line);
  } catch {
    return [];
  }

  if (!isRecord(event) || event.type !== "match" || !isRecord(event.data)) {
    return [];
  }

  const data = event.data;
  if (!isRecord(data.path) || typeof data.path.text !== "string") {
    return [];
  }

  if (!isRecord(data.lines) || typeof data.lines.text !== "string") {
    return [];
  }

  if (typeof data.line_number !== "number") {
    return [];
  }

  const path = normalizeResultPath(root, data.path.text);
  const lineText = trimTrailingNewline(data.lines.text);
  const lineNumber = data.line_number;
  const submatches = Array.isArray(data.submatches)
    ? data.submatches.filter(
        (submatch) =>
          isRecord(submatch) &&
          typeof submatch.start === "number" &&
          typeof submatch.end === "number",
      )
    : [];
  const matches =
    submatches.length > 0
      ? submatches
      : [
          {
            start: 0,
            end: Buffer.byteLength(lineText, "utf8"),
          },
        ];

  return matches.map((submatch, index) => {
    const start = textPositionAtByteOffset(lineText, submatch.start as number);
    const end = textPositionAtByteOffset(lineText, submatch.end as number);

    return {
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
  });
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
