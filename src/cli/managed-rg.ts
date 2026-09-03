import { realpathSync } from "node:fs";
import { basename, dirname, isAbsolute, relative, resolve } from "node:path";
import { parseArgs } from "./args.js";
import type { CliOptions, ParsedArgs } from "./types.js";

const MANAGED_RG_OPTION_KEYS = new Set<keyof CliOptions>([
  "rg",
  "rgCompatibilityOptions",
  "rgOptions",
  "rgPaths",
  "globs",
  "insensitiveGlobs",
  "fileTypes",
  "excludedFileTypes",
  "hidden",
  "noIgnore",
  "ignoreFiles",
  "maxDepth",
  "maxFileSizeBytes",
  "follow",
]);

type ManagedRgCommandTokens = {
  argv: string[];
  limit?: number;
};

function scanManagedRgCommand(command: string): string[] {
  const tokens: string[] = [];
  let token = "";
  let tokenStarted = false;
  let quote: "'" | '"' | undefined;
  let escaping = false;

  const finishToken = () => {
    if (!tokenStarted) {
      return;
    }
    tokens.push(token);
    token = "";
    tokenStarted = false;
  };

  for (let index = 0; index < command.length; index++) {
    const character = command[index]!;

    if (character === "\0") {
      throw new Error("rg command cannot contain NUL characters.");
    }
    if (character === "\n" || character === "\r") {
      throw new Error("rg command must be a single command on one line.");
    }

    if (escaping) {
      token += character;
      tokenStarted = true;
      escaping = false;
      continue;
    }

    if (quote === "'") {
      if (character === "'") {
        quote = undefined;
      } else {
        token += character;
      }
      tokenStarted = true;
      continue;
    }

    if (quote === '"') {
      if (character === '"') {
        quote = undefined;
      } else if (character === "\\") {
        const nextCharacter = command[index + 1];
        if (nextCharacter === undefined) {
          throw new Error("rg command ends with an incomplete escape.");
        }
        if ('"\\$`'.includes(nextCharacter)) {
          token += nextCharacter;
          index += 1;
        } else {
          token += character;
        }
      } else if (character === "`" || startsExpansion(command, index)) {
        throw new Error("rg command does not support shell expansion.");
      } else {
        token += character;
      }
      tokenStarted = true;
      continue;
    }

    if (/\s/u.test(character)) {
      finishToken();
      continue;
    }
    if (character === "'" || character === '"') {
      quote = character;
      tokenStarted = true;
      continue;
    }
    if (character === "\\") {
      // Backslashes are path separators in Windows shells, rather than generic
      // escape characters. Keep them so copied rg paths such as `src\\cli`
      // continue to point at the intended directory.
      if (process.platform === "win32") {
        token += character;
      } else {
        escaping = true;
      }
      tokenStarted = true;
      continue;
    }
    if (character === "|" || character === ">") {
      finishToken();
      tokens.push(character);
      continue;
    }
    if ("&;<>()".includes(character)) {
      throw new Error(
        `rg command does not support shell operator ${JSON.stringify(character)}.`,
      );
    }
    if (character === "`" || startsExpansion(command, index)) {
      throw new Error("rg command does not support shell expansion.");
    }

    token += character;
    tokenStarted = true;
  }

  if (escaping) {
    throw new Error("rg command ends with an incomplete escape.");
  }
  if (quote) {
    throw new Error(`rg command has an unclosed ${quote} quote.`);
  }
  finishToken();

  return tokens;
}

export function tokenizeManagedRgCommand(command: string): string[] {
  return normalizeManagedRgShellSuffix(scanManagedRgCommand(command)).argv;
}

function normalizeManagedRgShellSuffix(
  scannedTokens: readonly string[],
): ManagedRgCommandTokens {
  const tokens = [...scannedTokens];
  let limit: number | undefined;

  const headPipeIndex = tokens.lastIndexOf("|");
  if (headPipeIndex >= 0) {
    const suffix = tokens.slice(headPipeIndex + 1);
    limit = parseHeadLimit(suffix);
    tokens.splice(headPipeIndex);
  }

  if (
    tokens.length >= 3 &&
    tokens.at(-3) === "2" &&
    tokens.at(-2) === ">" &&
    tokens.at(-1) === "/dev/null"
  ) {
    tokens.splice(-3);
  }

  const unsupportedOperator = tokens.find(
    (token) => token === "|" || token === ">",
  );
  if (unsupportedOperator) {
    throw new Error(
      `rg command does not support shell operator ${JSON.stringify(unsupportedOperator)}.`,
    );
  }

  return {
    argv: normalizeManagedRgCompatibilityTokens(tokens),
    limit,
  };
}

function parseHeadLimit(tokens: readonly string[]): number {
  if (tokens[0] !== "head") {
    throw new Error(
      `rg command only supports a trailing "| head", "| head -N", or "| head -n N" output bound.`,
    );
  }

  let raw: string | undefined;
  if (tokens.length === 1) {
    raw = "10";
  } else if (tokens.length === 2 && /^-\d+$/u.test(tokens[1]!)) {
    raw = tokens[1]!.slice(1);
  } else if (
    tokens.length === 3 &&
    tokens[1] === "-n" &&
    /^\d+$/u.test(tokens[2]!)
  ) {
    raw = tokens[2];
  }

  if (!raw) {
    throw new Error(
      `rg command only supports a trailing "| head", "| head -N", or "| head -n N" output bound.`,
    );
  }

  const limit = Number(raw);
  if (!Number.isSafeInteger(limit) || limit <= 0) {
    throw new Error("rg command head limit must be a positive safe integer.");
  }
  return limit;
}

function normalizeManagedRgCompatibilityTokens(
  scannedTokens: readonly string[],
): string[] {
  const tokens: string[] = [];
  let filesWithMatches = false;

  for (const token of scannedTokens) {
    if (token === "-l" || token === "--files-with-matches") {
      filesWithMatches = true;
      continue;
    }
    if (/^-[^-]+$/u.test(token) && token.includes("l")) {
      const withoutFilesWithMatches = token.replaceAll("l", "");
      if (withoutFilesWithMatches !== "-") {
        tokens.push(withoutFilesWithMatches);
      }
      filesWithMatches = true;
      continue;
    }
    tokens.push(token);
  }

  if (filesWithMatches) {
    tokens.splice(1, 0, "--max-count", "1");
  }

  const hyphenPatternIndex = tokens.findIndex(
    (token, index) => index > 0 && token !== "--" && /^-{3,}/u.test(token),
  );
  if (
    hyphenPatternIndex > 0 &&
    !tokens.slice(1, hyphenPatternIndex).includes("--")
  ) {
    tokens.splice(hyphenPatternIndex, 0, "--");
  }

  return tokens;
}

function validateManagedRgTokens(tokens: readonly string[]): void {
  if (tokens[0] !== "rg") {
    throw new Error('rg command must start with "rg".');
  }
  if (tokens.length === 1) {
    throw new Error("rg command requires a pattern.");
  }
}

export function parseManagedRgCommand(
  root: string,
  command: string,
): { queries: string[]; options: CliOptions } {
  const normalizedCommand = normalizeManagedRgShellSuffix(
    scanManagedRgCommand(command),
  );
  const argv = normalizedCommand.argv;
  validateManagedRgTokens(argv);
  const parsed = parseArgs(["query", "--rg", ...argv.slice(1)]);
  assertOnlyManagedRgOptions(parsed.options);
  const normalized = normalizeManagedRgInput(parsed);
  normalized.queries = normalized.queries
    .map((query) => query.trim())
    .filter((query) => query.length > 0);

  if (
    normalized.queries.length === 0 &&
    (normalized.options.rgOptions?.patternFiles?.length ?? 0) === 0
  ) {
    throw new Error("rg command requires a non-empty pattern.");
  }

  assertRootScopedPaths(root, normalized.options);
  return {
    queries: normalized.queries,
    options: {
      ...normalized.options,
      // Match ripgrep semantics: the search is exhaustive unless the caller
      // explicitly supplies a trailing `head` output bound.
      limit: normalizedCommand.limit,
    },
  };
}

export function normalizeManagedRgInput(parsed: ParsedArgs): {
  queries: string[];
  options: CliOptions;
} {
  const explicitPatterns = parsed.options.rgOptions?.patterns ?? [];
  const hasPatternFiles =
    (parsed.options.rgOptions?.patternFiles?.length ?? 0) > 0;
  const queries =
    explicitPatterns.length > 0 || hasPatternFiles
      ? explicitPatterns
      : parsed.positionals.slice(0, 1);
  const paths =
    explicitPatterns.length > 0 || hasPatternFiles
      ? parsed.positionals
      : parsed.positionals.slice(1);

  return {
    queries,
    options: {
      ...parsed.options,
      rgPaths: paths.length > 0 ? paths : undefined,
    },
  };
}

function startsExpansion(command: string, index: number): boolean {
  return (
    command[index] === "$" &&
    (command[index + 1] === "(" || command[index + 1] === "{")
  );
}

function assertOnlyManagedRgOptions(options: CliOptions): void {
  const unsupported = Object.keys(options).find(
    (key) => !MANAGED_RG_OPTION_KEYS.has(key as keyof CliOptions),
  );
  if (unsupported) {
    throw new Error(
      `rg command option ${JSON.stringify(`--${toKebabCase(unsupported)}`)} is not supported by the MCP tool.`,
    );
  }
}

function assertRootScopedPaths(root: string, options: CliOptions): void {
  if (options.follow) {
    throw new Error(
      'rg command option "--follow" is not supported by the MCP tool.',
    );
  }

  const resolvedRoot = resolve(root);
  const canonicalRoot = resolveThroughExistingAncestor(resolvedRoot);
  for (const path of options.rgPaths ?? []) {
    assertRootScopedPath(resolvedRoot, canonicalRoot, path, "search path");
  }
  for (const path of options.ignoreFiles ?? []) {
    assertRootScopedPath(resolvedRoot, canonicalRoot, path, "ignore file");
  }
  for (const path of options.rgOptions?.patternFiles ?? []) {
    if (path === "-") {
      throw new Error("rg command cannot read patterns from stdin.");
    }
    assertRootScopedPath(resolvedRoot, canonicalRoot, path, "pattern file");
  }
}

function assertRootScopedPath(
  resolvedRoot: string,
  canonicalRoot: string,
  path: string,
  label: string,
): void {
  const resolvedPath = resolve(resolvedRoot, path);
  if (pathEscapesRoot(resolvedRoot, resolvedPath)) {
    throw new Error(`${label} must stay within root: ${path}`);
  }

  const canonicalPath = resolveThroughExistingAncestor(resolvedPath);
  if (pathEscapesRoot(canonicalRoot, canonicalPath)) {
    throw new Error(`${label} resolves outside root: ${path}`);
  }
}

function pathEscapesRoot(root: string, path: string): boolean {
  const relativePath = relative(root, path);
  return (
    relativePath === ".." ||
    relativePath.startsWith("../") ||
    relativePath.startsWith("..\\") ||
    isAbsolute(relativePath)
  );
}

function resolveThroughExistingAncestor(path: string): string {
  let current = path;
  const missingSegments: string[] = [];

  while (true) {
    try {
      return resolve(realpathSync(current), ...missingSegments);
    } catch (error) {
      if (!isMissingPathError(error)) {
        throw error;
      }
    }

    const parent = dirname(current);
    if (parent === current) {
      return path;
    }
    missingSegments.unshift(basename(current));
    current = parent;
  }
}

function isMissingPathError(error: unknown): boolean {
  if (
    !error ||
    typeof error !== "object" ||
    !("code" in error) ||
    typeof error.code !== "string"
  ) {
    return false;
  }
  return error.code === "ENOENT" || error.code === "ENOTDIR";
}

function toKebabCase(value: string): string {
  return value.replace(/[A-Z]/g, (character) => `-${character.toLowerCase()}`);
}
