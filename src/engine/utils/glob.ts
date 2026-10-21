import {
  chargeGlobWork,
  checkGlobLength,
  labeledGlobError,
} from "./glob-budget.js";
import { compileGlob } from "./glob-matcher.js";

export function normalizePathPattern(pattern: string): string {
  checkGlobLength(pattern, "pattern");
  chargeGlobWork(pattern.length);
  let normalized = pattern.trim().replaceAll("\\", "/").replace(/\/+/g, "/");

  if (isAbsolutePathPattern(normalized)) {
    return normalized;
  }

  while (normalized.startsWith("./")) {
    normalized = normalized.slice(2);
  }

  return normalized;
}

export function normalizePathForMatch(path: string): string {
  checkGlobLength(path, "path");
  chargeGlobWork(path.length);
  return path.replaceAll("\\", "/").replace(/\/+/g, "/");
}

export function isAbsolutePathPattern(pattern: string): boolean {
  return pattern.startsWith("/") || /^[A-Za-z]:\//.test(pattern);
}

export function hasPathGlob(pattern: string): boolean {
  return (
    pattern.includes("*") || pattern.includes("?") || pattern.includes("[")
  );
}

export function pathPatternMatches(pattern: string, path: string): boolean {
  return pathPatternMatchesWithCase(pattern, path, false);
}

/**
 * Match a pre-normalized pattern against a pre-normalized path. Callers that
 * normalize once per operation (for example parsed ignore rules and one
 * normalization per candidate path) use this to avoid repeated normalization
 * charges and work.
 */
export function pathPatternMatchesPrepared(
  normalizedPattern: string,
  normalizedPath: string,
  caseInsensitive = false,
): boolean {
  if (normalizedPattern.length === 0) {
    return false;
  }

  if (hasPathGlob(normalizedPattern)) {
    return globPatternMatches(
      normalizedPattern,
      normalizedPath,
      caseInsensitive,
    );
  }

  const candidate = caseInsensitive
    ? normalizedPath.toLowerCase()
    : normalizedPath;
  const expected = caseInsensitive
    ? normalizedPattern.toLowerCase()
    : normalizedPattern;
  const expectedPrefix = expected.endsWith("/") ? expected : `${expected}/`;

  return candidate === expected || candidate.startsWith(expectedPrefix);
}

/**
 * Compiled-weight estimate for one active rule, used for the aggregate
 * active-rule bound and per-path allowances. Literal patterns never reach the
 * compiled matcher under path-prefix semantics; charge their raw length instead.
 */
export function globPatternWeight(pattern: string, label?: string): number {
  if (!hasPathGlob(pattern)) {
    return pattern.length;
  }
  return compiledGlobWeight(pattern, false, label);
}

/**
 * Compiled weight for a pattern applied with ripgrep glob semantics, which
 * always compiles the matcher — literal patterns included — so their weight
 * is the compiled matcher's, not the raw pattern length. Case-insensitive
 * patterns must charge the case-insensitive compilation.
 */
export function ripgrepPatternWeight(
  pattern: string,
  caseInsensitive = false,
  label?: string,
): number {
  return compiledGlobWeight(pattern, caseInsensitive, label);
}

function compiledGlobWeight(
  pattern: string,
  caseInsensitive: boolean,
  label: string | undefined,
): number {
  try {
    return compileGlob(pattern, caseInsensitive).weight;
  } catch (error) {
    if (label === undefined) throw error;
    throw labeledGlobError(label, pattern, error);
  }
}

export function pathPatternMatchesCaseInsensitive(
  pattern: string,
  path: string,
): boolean {
  return pathPatternMatchesWithCase(pattern, path, true);
}

export function ripgrepGlobMatches(pattern: string, path: string): boolean {
  return ripgrepGlobMatchesWithCase(pattern, path, false);
}

export function ripgrepGlobMatchesCaseInsensitive(
  pattern: string,
  path: string,
): boolean {
  return ripgrepGlobMatchesWithCase(pattern, path, true);
}

function ripgrepGlobMatchesWithCase(
  pattern: string,
  path: string,
  caseInsensitive: boolean,
): boolean {
  const normalizedPattern = normalizePathPattern(pattern);
  if (normalizedPattern.length === 0) {
    return false;
  }

  return globPatternMatches(
    normalizedPattern,
    normalizePathForMatch(path),
    caseInsensitive,
  );
}

function pathPatternMatchesWithCase(
  pattern: string,
  path: string,
  caseInsensitive: boolean,
): boolean {
  const normalizedPattern = normalizePathPattern(pattern);
  const normalizedPath = normalizePathForMatch(path);

  if (normalizedPattern.length === 0) {
    return false;
  }

  if (hasPathGlob(normalizedPattern)) {
    return globPatternMatches(
      normalizedPattern,
      normalizedPath,
      caseInsensitive,
    );
  }

  const candidate = caseInsensitive
    ? normalizedPath.toLowerCase()
    : normalizedPath;
  const expected = caseInsensitive
    ? normalizedPattern.toLowerCase()
    : normalizedPattern;
  const expectedPrefix = expected.endsWith("/") ? expected : `${expected}/`;

  return candidate === expected || candidate.startsWith(expectedPrefix);
}

export function pathPatternMightMatchDescendant(
  pattern: string,
  directoryPath: string,
): boolean {
  const normalizedPattern = normalizePathPattern(pattern);
  const normalizedDirectory = normalizePathForMatch(directoryPath).replace(
    /\/+$/,
    "",
  );
  if (normalizedDirectory.length === 0) {
    return true;
  }

  return (
    pathPatternMatches(pattern, normalizedDirectory) ||
    pathPatternMatches(
      pattern,
      `${normalizedDirectory}/__zvec_grep_descendant__`,
    ) ||
    patternPrefixMightMatchDescendant(normalizedPattern, normalizedDirectory)
  );
}

function globPatternMatches(
  pattern: string,
  path: string,
  caseInsensitive: boolean,
): boolean {
  if (pattern.endsWith("/**")) {
    const directoryPattern = pattern.slice(0, -3);
    if (compileGlob(directoryPattern, caseInsensitive).test(path)) {
      return true;
    }
  }

  return compileGlob(pattern, caseInsensitive).test(path);
}

function patternPrefixMightMatchDescendant(
  pattern: string,
  directoryPath: string,
): boolean {
  const directoryPrefix = `${directoryPath}/`;
  const variants = pattern.startsWith("**/")
    ? [pattern, pattern.slice(3)]
    : [pattern];

  for (const variant of variants) {
    if (!hasPathGlob(variant)) {
      if (variant.startsWith(directoryPrefix)) {
        return true;
      }
      continue;
    }

    const literalPrefix = literalPrefixBeforeFirstGlob(variant);
    if (
      literalPrefix.length > 0 &&
      (literalPrefix.startsWith(directoryPrefix) ||
        directoryPrefix.startsWith(literalPrefix))
    ) {
      return true;
    }
  }

  return false;
}

function literalPrefixBeforeFirstGlob(pattern: string): string {
  const indexes = [pattern.indexOf("*"), pattern.indexOf("?")].filter(
    (index) => index >= 0,
  );
  if (indexes.length === 0) {
    return pattern;
  }

  return pattern.slice(0, Math.min(...indexes));
}
