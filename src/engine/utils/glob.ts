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
  return pattern.includes("*") || pattern.includes("?");
}

export function pathPatternMatches(pattern: string, path: string): boolean {
  const normalizedPattern = normalizePathPattern(pattern);
  const normalizedPath = normalizePathForMatch(path);

  if (normalizedPattern.length === 0) {
    return false;
  }

  if (hasPathGlob(normalizedPattern)) {
    return globPatternMatches(normalizedPattern, normalizedPath);
  }

  const prefix = normalizedPattern.endsWith("/")
    ? normalizedPattern
    : `${normalizedPattern}/`;

  return (
    normalizedPath === normalizedPattern || normalizedPath.startsWith(prefix)
  );
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

function globPatternMatches(pattern: string, path: string): boolean {
  if (pattern.endsWith("/**")) {
    const directoryPattern = pattern.slice(0, -3);
    if (globToRegExp(directoryPattern).test(path)) {
      return true;
    }
  }

  return globToRegExp(pattern).test(path);
}

function globToRegExp(pattern: string): RegExp {
  let expression = pattern.includes("/") ? "^" : "^(?:.*/)?";

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
    } else {
      expression += escapeRegExp(char);
    }
  }

  return new RegExp(`${expression}$`);
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
