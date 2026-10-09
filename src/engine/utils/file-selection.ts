import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { setImmediate as defaultYieldToEventLoop } from "node:timers/promises";
import {
  checkGlobLength,
  checkGlobRuleCount,
  labeledGlobError,
  yieldGlobWorkIfNeeded,
} from "./glob-budget.js";
import {
  ripgrepGlobMatches,
  ripgrepGlobMatchesCaseInsensitive,
} from "./glob.js";

const execFileAsync = promisify(execFile);

export type FileTypePattern = {
  pattern: string;
  /** Request field and type name this pattern was expanded from. */
  origin: string;
};

export type FileTypePatterns = {
  include: readonly FileTypePattern[];
  exclude: readonly FileTypePattern[];
};

export type FileSelection = {
  globs?: readonly string[];
  insensitiveGlobs?: readonly string[];
};

let ripgrepTypeMapPromise:
  Promise<ReadonlyMap<string, readonly string[]>> | undefined;

const RIPGREP_FILE_TYPE_ALIASES: Readonly<Record<string, string>> = {
  bash: "sh",
  cjs: "js",
  cp: "cpp",
  cc: "cpp",
  cpp: "cpp",
  cxx: "cpp",
  hpp: "h",
  hxx: "h",
  hh: "h",
  h: "h",
  js: "js",
  jsx: "js",
  mjs: "js",
  markdown: "md",
  mdx: "md",
  pyi: "py",
  py: "py",
  rb: "ruby",
  rs: "rust",
  ts: "ts",
  tsx: "ts",
  yml: "yaml",
  zsh: "sh",
};

export async function resolveFileTypePatterns(
  includedTypes: readonly string[] | undefined,
  excludedTypes: readonly string[] | undefined,
): Promise<FileTypePatterns> {
  if (!includedTypes?.length && !excludedTypes?.length) {
    return { include: [], exclude: [] };
  }

  const types = await ripgrepTypeMap();
  return {
    include: resolveTypeNames(includedTypes, types, "fileTypes"),
    exclude: resolveTypeNames(excludedTypes, types, "excludedFileTypes"),
  };
}

export type SelectionYieldOptions = {
  signal?: AbortSignal;
  yieldToEventLoop?: () => Promise<unknown>;
};

export async function matchesFileSelection(
  path: string,
  selection: FileSelection,
  types: FileTypePatterns,
  options: SelectionYieldOptions = {},
): Promise<boolean> {
  checkGlobRuleCount(
    (selection.globs?.length ?? 0) +
      (selection.insensitiveGlobs?.length ?? 0) +
      types.include.length +
      types.exclude.length,
  );
  const yieldFn = options.yieldToEventLoop ?? defaultYieldToEventLoop;
  const includedByGlob = await matchesOrderedGlobs(
    path,
    selection,
    yieldFn,
    options.signal,
  );
  const includedByType =
    types.include.length === 0 ||
    (await someTypeMatches(types.include, path, yieldFn, options.signal));
  const excludedByType = await someTypeMatches(
    types.exclude,
    path,
    yieldFn,
    options.signal,
  );

  return includedByGlob && includedByType && !excludedByType;
}

async function someTypeMatches(
  entries: readonly FileTypePattern[],
  path: string,
  yieldFn: () => Promise<unknown>,
  signal: AbortSignal | undefined,
): Promise<boolean> {
  for (const entry of entries) {
    await yieldGlobWorkIfNeeded(yieldFn, signal);
    if (applyLabeledPattern(entry.origin, entry.pattern, path, false)) {
      return true;
    }
  }
  return false;
}

function applyLabeledPattern(
  label: string,
  pattern: string,
  path: string,
  caseInsensitive: boolean,
): boolean {
  try {
    return caseInsensitive
      ? ripgrepGlobMatchesCaseInsensitive(pattern, path)
      : ripgrepGlobMatches(pattern, path);
  } catch (error) {
    throw labeledGlobError(label, pattern, error);
  }
}

async function matchesOrderedGlobs(
  path: string,
  selection: FileSelection,
  yieldFn: () => Promise<unknown>,
  signal: AbortSignal | undefined,
): Promise<boolean> {
  const rules = [
    ...(selection.globs ?? []).map((pattern, index) => ({
      label: `globs[${index}]`,
      pattern,
      caseInsensitive: false,
    })),
    ...(selection.insensitiveGlobs ?? []).map((pattern, index) => ({
      label: `insensitiveGlobs[${index}]`,
      pattern,
      caseInsensitive: true,
    })),
  ]
    .map((rule) => {
      checkGlobLength(rule.pattern, "pattern");
      return { ...rule, pattern: rule.pattern.trim() };
    })
    .filter((rule) => rule.pattern.length > 0);
  const hasPositiveRule = rules.some((rule) => !rule.pattern.startsWith("!"));
  let included = !hasPositiveRule;

  for (const rule of rules) {
    await yieldGlobWorkIfNeeded(yieldFn, signal);
    const negated = rule.pattern.startsWith("!");
    const pattern = negated ? rule.pattern.slice(1).trim() : rule.pattern;
    if (!pattern) {
      continue;
    }
    const matches = applyLabeledPattern(
      rule.label,
      pattern,
      path,
      rule.caseInsensitive,
    );
    if (matches) {
      included = !negated;
    }
  }

  return included;
}

function resolveTypeNames(
  names: readonly string[] | undefined,
  types: ReadonlyMap<string, readonly string[]>,
  originField: string,
): FileTypePattern[] {
  const patterns: FileTypePattern[] = [];
  for (const rawName of names ?? []) {
    const name = rawName.trim().toLowerCase();
    if (!name) {
      continue;
    }
    const origin = `${originField} "${rawName}"`;
    if (name === "all") {
      patterns.push({ pattern: "**", origin });
      continue;
    }
    const typeName = resolveRipgrepTypeName(name, types);
    const typePatterns = types.get(typeName);
    if (!typePatterns) {
      throw new Error(`Unknown ripgrep file type: ${rawName}`);
    }
    for (const pattern of typePatterns) {
      patterns.push({ pattern, origin });
    }
  }
  const seen = new Set<string>();
  return patterns.filter((entry) => {
    if (seen.has(entry.pattern)) return false;
    seen.add(entry.pattern);
    return true;
  });
}

function resolveRipgrepTypeName(
  name: string,
  types: ReadonlyMap<string, readonly string[]>,
): string {
  if (types.has(name)) {
    return name;
  }

  const extensionName = name.startsWith(".") ? name.slice(1) : name;
  const alias = RIPGREP_FILE_TYPE_ALIASES[extensionName];
  if (alias && types.has(alias)) {
    return alias;
  }

  return name;
}

async function ripgrepTypeMap(): Promise<
  ReadonlyMap<string, readonly string[]>
> {
  ripgrepTypeMapPromise ??= loadRipgrepTypeMap();
  return ripgrepTypeMapPromise;
}

async function loadRipgrepTypeMap(): Promise<
  ReadonlyMap<string, readonly string[]>
> {
  const commands: string[] = [];
  try {
    const { rgPath } = await import("@vscode/ripgrep");
    commands.push(rgPath);
  } catch {
    // The system rg fallback below remains available.
  }
  commands.push("rg");

  let lastError: unknown;
  for (const command of [...new Set(commands)]) {
    try {
      const { stdout } = await execFileAsync(command, ["--type-list"], {
        encoding: "utf8",
        maxBuffer: 4 * 1024 * 1024,
      });
      return parseRipgrepTypeList(stdout);
    } catch (error) {
      lastError = error;
    }
  }

  throw new Error("Unable to load ripgrep file types", { cause: lastError });
}

function parseRipgrepTypeList(
  output: string,
): ReadonlyMap<string, readonly string[]> {
  const types = new Map<string, readonly string[]>();
  for (const line of output.split(/\r?\n/)) {
    const separator = line.indexOf(":");
    if (separator <= 0) {
      continue;
    }
    const name = line.slice(0, separator).trim().toLowerCase();
    const patterns = line
      .slice(separator + 1)
      .split(",")
      .map((pattern) => pattern.trim())
      .filter(Boolean);
    if (name && patterns.length > 0) {
      types.set(name, patterns);
    }
  }
  return types;
}
