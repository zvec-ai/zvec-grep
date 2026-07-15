import { open, readFile, readdir, stat } from "node:fs/promises";
import { basename, join, relative } from "node:path";
import type { FileInfo, RootPath } from "../../../types.js";
import { detectFileType } from "../../../files/file-type.js";
import { sha256Text } from "../../../utils/hash.js";
import {
  normalizePathPattern,
  pathPatternMatches,
  pathPatternMightMatchDescendant,
} from "../../../utils/glob.js";
import { normalizePath, toDisplayPath } from "../../../utils/path.js";
import {
  matchesRootExcludePatterns,
  matchesRootPatterns,
  normalizeRootPath,
  validateRootPaths,
} from "../root-paths.js";

const DEFAULT_MAX_FILE_SIZE_BYTES = 1 * 1024 * 1024;
const BINARY_SNIFF_BYTES = 8192;
const BINARY_CONTROL_CHAR_RATIO = 0.3;

const DEFAULT_IGNORED_DIRECTORY_NAMES = [
  "node_modules",
  "vendor",
  "thirdparty",
  "third_party",
  "external",
  "deps",
  "dist",
  "build",
  "out",
  "target",
  "coverage",
  "generated",
  "__pycache__",
  "venv",
  ".venv",
  "env",
  ".tox",
  ".eggs",
  "Pods",
  ".next",
  ".nuxt",
  ".svelte-kit",
  ".turbo",
  ".vite",
  ".parcel-cache",
  ".cache",
  ".gradle",
  ".pytest_cache",
  ".mypy_cache",
  ".ruff_cache",
  "tmp",
  "temp",
  "logs",
] as const;

const HARD_SKIP_HIDDEN_NAMES = new Set([".git", ".zvec-grep"]);

type IgnoreRule = {
  basePath: string;
  pattern: string;
  negated: boolean;
  directoryOnly: boolean;
  anchored: boolean;
  hasSlash: boolean;
};

type IgnoreMatch = {
  ignored: boolean;
  matchedNegation: boolean;
  matchedRule?: IgnoreRule;
};

const DEFAULT_IGNORE_RULES: readonly IgnoreRule[] =
  DEFAULT_IGNORED_DIRECTORY_NAMES.map((pattern) => ({
    basePath: "",
    pattern,
    negated: false,
    directoryOnly: true,
    anchored: false,
    hasSlash: false,
  }));

export type ScanResult = {
  files: FileInfo[];
};

export async function scanRootPaths(
  collectionId: string,
  rootPaths: readonly RootPath[],
): Promise<ScanResult> {
  const validatedRootPaths = validateRootPaths(rootPaths);
  const files: FileInfo[] = [];

  for (const rootPath of validatedRootPaths) {
    await scanRootPath(collectionId, rootPath, files);
  }

  return { files };
}

async function scanRootPath(
  collectionId: string,
  rootPath: RootPath,
  files: FileInfo[],
): Promise<void> {
  const root = normalizeRootPath(rootPath);
  const info = await stat(root.absolutePath).catch(() => null);

  if (!info) {
    return;
  }

  if (info.isFile()) {
    const file = await readFileInfo(collectionId, root, root.absolutePath);
    if (file) {
      files.push(file);
    }
    return;
  }

  if (!info.isDirectory()) {
    return;
  }

  if (HARD_SKIP_HIDDEN_NAMES.has(basename(root.absolutePath))) {
    return;
  }

  await walk(
    collectionId,
    root,
    root.absolutePath,
    files,
    DEFAULT_IGNORE_RULES,
  );
}

async function walk(
  collectionId: string,
  rootPath: RootPath,
  currentPath: string,
  files: FileInfo[],
  parentIgnoreRules: readonly IgnoreRule[],
): Promise<void> {
  let entries;
  const ignoreRules = [
    ...parentIgnoreRules,
    ...(await readGitIgnoreRules(rootPath, currentPath)),
  ];

  try {
    entries = await readdir(currentPath, { withFileTypes: true });
  } catch {
    return;
  }

  for (const entry of entries) {
    const absolutePath = join(currentPath, entry.name);
    const relativePath = toDisplayPath(
      relative(rootPath.absolutePath, absolutePath),
    );

    if (entry.isDirectory()) {
      if (!rootPath.recursive) {
        continue;
      }

      const ignoreMatch = matchIgnoreRules(relativePath, true, ignoreRules);
      if (
        HARD_SKIP_HIDDEN_NAMES.has(entry.name) ||
        matchesRootExcludePatterns(relativePath, rootPath) ||
        (ignoreMatch.ignored &&
          !ignoredPathExplicitlyIncluded(
            relativePath,
            rootPath,
            ignoreMatch,
          )) ||
        shouldSkipHiddenDirectory(entry.name, relativePath, rootPath)
      ) {
        continue;
      }

      if (
        (await isNestedGitRepositoryDirectory(absolutePath)) &&
        !nestedGitRepositoryExplicitlyIncluded(relativePath, rootPath)
      ) {
        continue;
      }

      await walk(collectionId, rootPath, absolutePath, files, ignoreRules);
      continue;
    }

    if (!entry.isFile()) {
      continue;
    }

    if (HARD_SKIP_HIDDEN_NAMES.has(entry.name)) {
      continue;
    }

    const ignoreMatch = matchIgnoreRules(relativePath, false, ignoreRules);
    if (
      ignoreMatch.ignored &&
      !ignoredPathExplicitlyIncluded(relativePath, rootPath, ignoreMatch)
    ) {
      continue;
    }

    if (shouldSkipHiddenFile(entry.name, relativePath, rootPath)) {
      continue;
    }

    if (!matchesRootPatterns(relativePath, rootPath)) {
      continue;
    }

    const file = await readFileInfo(collectionId, rootPath, absolutePath);
    if (file) {
      files.push(file);
    }
  }
}

function isHiddenName(name: string): boolean {
  return name.startsWith(".") && name !== "." && name !== "..";
}

async function readGitIgnoreRules(
  rootPath: RootPath,
  currentPath: string,
): Promise<IgnoreRule[]> {
  const content = await readFile(join(currentPath, ".gitignore"), "utf8").catch(
    () => null,
  );
  if (content === null) {
    return [];
  }

  const basePath = toDisplayPath(relative(rootPath.absolutePath, currentPath));
  return parseGitIgnoreRules(content, basePath);
}

function parseGitIgnoreRules(content: string, basePath: string): IgnoreRule[] {
  const rules: IgnoreRule[] = [];

  for (const rawLine of content.split(/\r?\n/)) {
    const rule = parseGitIgnoreRule(rawLine, basePath);
    if (rule) {
      rules.push(rule);
    }
  }

  return rules;
}

function parseGitIgnoreRule(line: string, basePath: string): IgnoreRule | null {
  let pattern = line.trim();
  if (pattern.length === 0 || pattern.startsWith("#")) {
    return null;
  }

  let negated = false;
  if (pattern.startsWith("\\#") || pattern.startsWith("\\!")) {
    pattern = pattern.slice(1);
  } else if (pattern.startsWith("!")) {
    negated = true;
    pattern = pattern.slice(1).trim();
  }

  if (pattern.length === 0) {
    return null;
  }

  const directoryOnly = pattern.endsWith("/");
  pattern = directoryOnly ? pattern.replace(/\/+$/, "") : pattern;
  const anchored = pattern.startsWith("/");
  pattern = anchored ? pattern.replace(/^\/+/, "") : pattern;
  pattern = normalizePathPattern(pattern);

  if (pattern.length === 0) {
    return null;
  }

  return {
    basePath,
    pattern,
    negated,
    directoryOnly,
    anchored,
    hasSlash: pattern.includes("/"),
  };
}

function matchIgnoreRules(
  relativePath: string,
  isDirectory: boolean,
  rules: readonly IgnoreRule[],
): IgnoreMatch {
  let ignored = false;
  let matchedNegation = false;
  let matchedRule: IgnoreRule | undefined;

  for (const rule of rules) {
    if (!ignoreRuleMatches(rule, relativePath, isDirectory)) {
      continue;
    }

    ignored = !rule.negated;
    matchedNegation = rule.negated;
    matchedRule = rule;
  }

  return {
    ignored,
    matchedNegation,
    matchedRule,
  };
}

function ignoredPathExplicitlyIncluded(
  relativePath: string,
  rootPath: RootPath,
  ignoreMatch: IgnoreMatch,
): boolean {
  if (!ignoreMatch.ignored || !ignoreMatch.matchedRule || !rootPath.include) {
    return false;
  }

  return rootPath.include.some((pattern) =>
    includePatternNamesIgnoredPath(
      pattern,
      relativePath,
      ignoreMatch.matchedRule!.pattern,
    ),
  );
}

function includePatternNamesIgnoredPath(
  includePattern: string,
  relativePath: string,
  ignoredPattern: string,
): boolean {
  const normalizedInclude = normalizePathPattern(includePattern);
  const normalizedRelativePath = normalizePathPattern(relativePath);
  const ignoredSegments = ignoredPattern.split("/");

  if (
    !pathPatternMightMatchDescendant(
      normalizedInclude,
      normalizedRelativePath,
    ) &&
    !pathPatternMatches(normalizedInclude, normalizedRelativePath)
  ) {
    return false;
  }

  return normalizedInclude
    .split("/")
    .some((segment) =>
      ignoredSegments.some((ignoredSegment) =>
        includeSegmentNamesIgnoredPath(segment, ignoredSegment),
      ),
    );
}

function includeSegmentNamesIgnoredPath(
  segment: string,
  ignoredSegment: string,
): boolean {
  if (segment === "*" || segment === "**" || segment === "?") {
    return false;
  }

  return segmentMatches(segment, ignoredSegment);
}

function ignoreRuleMatches(
  rule: IgnoreRule,
  relativePath: string,
  isDirectory: boolean,
): boolean {
  const path = relativeToIgnoreRuleBase(relativePath, rule.basePath);
  if (path === null || path.length === 0) {
    return false;
  }

  if (rule.directoryOnly) {
    if (rule.anchored || rule.hasSlash) {
      return pathPatternMatches(rule.pattern, path);
    }

    return pathContainsMatchingSegment(path, rule.pattern);
  }

  if (rule.anchored || rule.hasSlash) {
    return pathPatternMatches(rule.pattern, path);
  }

  if (isDirectory && pathContainsMatchingSegment(path, rule.pattern)) {
    return true;
  }

  return segmentMatches(rule.pattern, basename(path));
}

function relativeToIgnoreRuleBase(
  relativePath: string,
  basePath: string,
): string | null {
  if (basePath.length === 0) {
    return relativePath;
  }

  if (relativePath === basePath) {
    return "";
  }

  const prefix = `${basePath}/`;
  return relativePath.startsWith(prefix)
    ? relativePath.slice(prefix.length)
    : null;
}

function pathContainsMatchingSegment(path: string, pattern: string): boolean {
  return path.split("/").some((segment) => segmentMatches(pattern, segment));
}

function segmentMatches(pattern: string, segment: string): boolean {
  return pathPatternMatches(pattern, segment);
}

function shouldSkipHiddenDirectory(
  name: string,
  relativePath: string,
  rootPath: RootPath,
): boolean {
  if (!isHiddenName(name)) {
    return false;
  }

  return !hasIncludeDescendant(relativePath, rootPath.include);
}

function shouldSkipHiddenFile(
  name: string,
  relativePath: string,
  rootPath: RootPath,
): boolean {
  return (
    isHiddenName(name) &&
    !hasExplicitHiddenFileInclude(relativePath, rootPath.include)
  );
}

async function isNestedGitRepositoryDirectory(
  absolutePath: string,
): Promise<boolean> {
  const marker = await stat(join(absolutePath, ".git")).catch(() => null);
  return marker !== null && (marker.isFile() || marker.isDirectory());
}

function nestedGitRepositoryExplicitlyIncluded(
  relativePath: string,
  rootPath: RootPath,
): boolean {
  if (!rootPath.include || rootPath.include.length === 0) {
    return false;
  }

  const normalizedRelativePath = normalizePathPattern(relativePath);
  return rootPath.include.some((pattern) => {
    const normalizedPattern = normalizePathPattern(pattern);
    return (
      pathPatternMatches(normalizedPattern, normalizedRelativePath) ||
      normalizedPattern.startsWith(`${normalizedRelativePath}/`)
    );
  });
}

function hasIncludeDescendant(
  relativePath: string,
  includePatterns: readonly string[] | undefined,
): boolean {
  if (!includePatterns || includePatterns.length === 0) {
    return false;
  }

  return includePatterns.some(
    (pattern) =>
      includePatternDeclaresHiddenDirectory(pattern, relativePath) &&
      pathPatternMightMatchDescendant(pattern, relativePath),
  );
}

function hasExplicitHiddenFileInclude(
  relativePath: string,
  includePatterns: readonly string[] | undefined,
): boolean {
  if (!includePatterns || includePatterns.length === 0) {
    return false;
  }

  return includePatterns.some(
    (pattern) =>
      includePatternDeclaresHiddenDirectory(pattern, relativePath) &&
      pathPatternMatches(pattern, relativePath),
  );
}

function includePatternDeclaresHiddenDirectory(
  pattern: string,
  relativePath: string,
): boolean {
  const name = basename(relativePath);
  if (!isHiddenName(name)) {
    return true;
  }

  return normalizePathPattern(pattern)
    .split("/")
    .some((segment) => hiddenPatternSegmentMatches(segment, name));
}

function hiddenPatternSegmentMatches(
  patternSegment: string,
  name: string,
): boolean {
  if (!patternSegment.startsWith(".")) {
    return false;
  }

  if (!patternSegment.includes("*") && !patternSegment.includes("?")) {
    return patternSegment === name;
  }

  return segmentGlobToRegExp(patternSegment).test(name);
}

function segmentGlobToRegExp(pattern: string): RegExp {
  let expression = "^";
  for (const char of pattern) {
    if (char === "*") {
      expression += "[^/]*";
    } else if (char === "?") {
      expression += "[^/]";
    } else {
      expression += escapeRegExp(char);
    }
  }

  return new RegExp(`${expression}$`);
}

function escapeRegExp(value: string): string {
  return value.replace(/[|\\{}()[\]^$+*?.]/g, "\\$&");
}

async function readFileInfo(
  collectionId: string,
  rootPath: RootPath,
  absolutePath: string,
): Promise<FileInfo | null> {
  const info = await stat(absolutePath).catch(() => null);

  if (!info || !info.isFile()) {
    return null;
  }

  if (info.size === 0 || info.size > DEFAULT_MAX_FILE_SIZE_BYTES) {
    return null;
  }

  const detected = detectFileType(absolutePath);
  if (!detected) {
    return null;
  }

  if (detected.kind !== "image" && (await isLikelyBinaryFile(absolutePath))) {
    return null;
  }

  return {
    id: makeFileId(collectionId, absolutePath),
    collectionId,
    absolutePath,
    relativePath:
      toDisplayPath(relative(rootPath.absolutePath, absolutePath)) ||
      basename(absolutePath),
    rootPath: rootPath.absolutePath,
    sizeBytes: info.size,
    lastModifiedTime: info.mtimeMs,
    kind: detected.kind,
    format: detected.format,
  };
}

async function isLikelyBinaryFile(path: string): Promise<boolean> {
  let handle;

  try {
    handle = await open(path, "r");
    const buffer = Buffer.alloc(BINARY_SNIFF_BYTES);
    const { bytesRead } = await handle.read(buffer, 0, buffer.length, 0);
    if (bytesRead === 0) {
      return false;
    }

    let suspicious = 0;
    for (let index = 0; index < bytesRead; index++) {
      const value = buffer[index];
      if (value === 0) {
        return true;
      }

      if (isSuspiciousControlByte(value)) {
        suspicious++;
      }
    }

    return suspicious / bytesRead > BINARY_CONTROL_CHAR_RATIO;
  } catch {
    return false;
  } finally {
    await handle?.close().catch(() => undefined);
  }
}

function isSuspiciousControlByte(value: number): boolean {
  return (
    value < 32 &&
    value !== 7 &&
    value !== 8 &&
    value !== 9 &&
    value !== 10 &&
    value !== 12 &&
    value !== 13 &&
    value !== 27
  );
}

function makeFileId(collectionId: string, absolutePath: string): string {
  return sha256Text(`${collectionId}\0${normalizePath(absolutePath)}`);
}
