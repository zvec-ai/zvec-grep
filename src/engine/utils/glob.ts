export function normalizePathPattern(pattern: string): string {
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
    if (globToRegExp(directoryPattern, caseInsensitive).test(path)) {
      return true;
    }
  }

  return globToRegExp(pattern, caseInsensitive).test(path);
}

function globToRegExp(pattern: string, caseInsensitive = false): RegExp {
  let expression = pattern.includes("/") ? "^" : "^(?:.*/)?";

  expression += globFragmentToRegExp(pattern);

  return new RegExp(`${expression}$`, caseInsensitive ? "i" : undefined);
}

function globFragmentToRegExp(pattern: string): string {
  let expression = "";

  for (let index = 0; index < pattern.length; index++) {
    const char = pattern[index];
    const next = pattern[index + 1];
    const afterNext = pattern[index + 2];

    if (char === "*" && next === "*" && afterNext === "/") {
      expression += "(?:.*/)?";
      index += 2;
    } else if (char === "*" && next === "*") {
      expression += ".*";
      index++;
    } else if (char === "*") {
      expression += "[^/]*";
    } else if (char === "?") {
      expression += "[^/]";
    } else if (char === "[") {
      const characterClass = readGlobCharacterClass(pattern, index);
      if (characterClass) {
        expression += characterClass.expression;
        index = characterClass.endIndex;
      } else {
        expression += "\\[";
      }
    } else if (char === "{") {
      const alternation = readGlobAlternation(pattern, index);
      if (alternation) {
        expression += `(?:${alternation.alternatives
          .map(globFragmentToRegExp)
          .join("|")})`;
        index = alternation.endIndex;
      } else {
        expression += "\\{";
      }
    } else {
      expression += escapeRegExp(char);
    }
  }

  return expression;
}

function readGlobAlternation(
  pattern: string,
  startIndex: number,
): { alternatives: string[]; endIndex: number } | undefined {
  const alternatives: string[] = [];
  let depth = 0;
  let alternativeStart = startIndex + 1;

  for (let index = startIndex + 1; index < pattern.length; index++) {
    const char = pattern[index];
    if (char === "{") {
      depth++;
      continue;
    }
    if (char === "}" && depth > 0) {
      depth--;
      continue;
    }
    if (char === "," && depth === 0) {
      alternatives.push(pattern.slice(alternativeStart, index));
      alternativeStart = index + 1;
      continue;
    }
    if (char === "}" && depth === 0) {
      if (alternatives.length === 0) {
        return undefined;
      }
      alternatives.push(pattern.slice(alternativeStart, index));
      return { alternatives, endIndex: index };
    }
  }

  return undefined;
}

function readGlobCharacterClass(
  pattern: string,
  startIndex: number,
): { expression: string; endIndex: number } | undefined {
  const endIndex = pattern.indexOf("]", startIndex + 1);
  if (endIndex < 0) {
    return undefined;
  }

  let content = pattern.slice(startIndex + 1, endIndex);
  if (!content || content === "!" || content === "^") {
    return undefined;
  }

  const negated = content.startsWith("!") || content.startsWith("^");
  if (negated) {
    content = content.slice(1);
  }
  content = content.replaceAll("\\", "\\\\").replaceAll("/", "\\/");

  return {
    expression: `[${negated ? "^" : ""}${content}]`,
    endIndex,
  };
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

function escapeRegExp(value: string): string {
  return value.replace(/[|\\{}()[\]^$+*?.]/g, "\\$&");
}
