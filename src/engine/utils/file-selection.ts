import { execFile } from "node:child_process";
import { promisify } from "node:util";
import {
  ripgrepGlobMatches,
  ripgrepGlobMatchesCaseInsensitive,
} from "./glob.js";

const execFileAsync = promisify(execFile);

export type FileTypePatterns = {
  include: readonly string[];
  exclude: readonly string[];
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
    include: resolveTypeNames(includedTypes, types),
    exclude: resolveTypeNames(excludedTypes, types),
  };
}

export function matchesFileSelection(
  path: string,
  selection: FileSelection,
  types: FileTypePatterns,
): boolean {
  const includedByGlob = matchesOrderedGlobs(path, selection);
  const includedByType =
    types.include.length === 0 ||
    types.include.some((glob) => ripgrepGlobMatches(glob, path));
  const excludedByType = types.exclude.some((glob) =>
    ripgrepGlobMatches(glob, path),
  );

  return includedByGlob && includedByType && !excludedByType;
}

function matchesOrderedGlobs(path: string, selection: FileSelection): boolean {
  const rules = [
    ...(selection.globs ?? []).map((pattern) => ({
      pattern,
      caseInsensitive: false,
    })),
    ...(selection.insensitiveGlobs ?? []).map((pattern) => ({
      pattern,
      caseInsensitive: true,
    })),
  ]
    .map((rule) => ({ ...rule, pattern: rule.pattern.trim() }))
    .filter((rule) => rule.pattern.length > 0);
  const hasPositiveRule = rules.some((rule) => !rule.pattern.startsWith("!"));
  let included = !hasPositiveRule;

  for (const rule of rules) {
    const negated = rule.pattern.startsWith("!");
    const pattern = negated ? rule.pattern.slice(1).trim() : rule.pattern;
    if (!pattern) {
      continue;
    }
    const matches = rule.caseInsensitive
      ? ripgrepGlobMatchesCaseInsensitive(pattern, path)
      : ripgrepGlobMatches(pattern, path);
    if (matches) {
      included = !negated;
    }
  }

  return included;
}

function resolveTypeNames(
  names: readonly string[] | undefined,
  types: ReadonlyMap<string, readonly string[]>,
): string[] {
  const patterns: string[] = [];
  for (const rawName of names ?? []) {
    const name = rawName.trim().toLowerCase();
    if (!name) {
      continue;
    }
    if (name === "all") {
      patterns.push("**");
      continue;
    }
    const typeName = resolveRipgrepTypeName(name, types);
    const typePatterns = types.get(typeName);
    if (!typePatterns) {
      throw new Error(`Unknown ripgrep file type: ${rawName}`);
    }
    patterns.push(...typePatterns);
  }
  return [...new Set(patterns)];
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
