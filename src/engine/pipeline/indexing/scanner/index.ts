import {
  lstat,
  open,
  readFile,
  readdir,
  realpath,
  stat,
} from "node:fs/promises";
import {
  basename,
  dirname,
  isAbsolute,
  join,
  relative,
  resolve,
  sep,
} from "node:path";
import type { FileInfo, RootPath } from "../../../types.js";
import { detectFileType } from "../../../file-type.js";
import { sha256Text } from "../../../utils/hash.js";
import {
  matchesFileSelection,
  resolveFileTypePatterns,
  type FileTypePatterns,
} from "../../../utils/file-selection.js";
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
const MAX_GITIGNORE_CACHE_ENTRIES = 4_096;

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
  "locale",
  "locales",
  "translations",
] as const;

const DEFAULT_IGNORED_FILE_PATTERNS = [
  // Cross-ecosystem dependency resolution output. These files remain
  // available to exact rg search and can be restored with an explicit include.
  "*.lock",
  "*.lockb",
  "*-lock.json",
  "*-lock.yaml",
  "npm-shrinkwrap.json",
  "go.sum",
  "*.resolved",
  // Generated localization, browser, and compiler artifacts.
  "*.po",
  "*.pot",
  "*.map",
  "*.min.*",
  "*.bundle.*",
  // General generated-source naming conventions outside generated directories.
  "*.generated.*",
  "*.gen.*",
  "*.designer.*",
  "*.pb.*",
  "*_pb2.*",
  "*.g.*",
  // Raster assets require an explicitly selected path to enter the semantic
  // index. Exact search behavior is unaffected.
  "*.gif",
  "*.jpeg",
  "*.jpg",
  "*.png",
  "*.webp",
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

const DEFAULT_IGNORE_RULES: readonly IgnoreRule[] = [
  ...DEFAULT_IGNORED_DIRECTORY_NAMES.map((pattern) => ({
    basePath: "",
    pattern,
    negated: false,
    directoryOnly: true,
    anchored: false,
    hasSlash: false,
  })),
  ...DEFAULT_IGNORED_FILE_PATTERNS.map((pattern) => ({
    basePath: "",
    pattern,
    negated: false,
    directoryOnly: false,
    anchored: false,
    hasSlash: false,
  })),
];

const gitIgnoreRuleCache = new Map<
  string,
  { content: string; rules: IgnoreRule[] }
>();

export type ScanResult = {
  files: FileInfo[];
};

export type ScanOptions = {
  signal?: AbortSignal;
};

export async function scanRootPaths(
  workspaceIndexId: string,
  rootPaths: readonly RootPath[],
  options: ScanOptions = {},
): Promise<ScanResult> {
  const validatedRootPaths = validateRootPaths(rootPaths);
  const files: FileInfo[] = [];

  for (const rootPath of validatedRootPaths) {
    throwIfAborted(options.signal);
    await scanRootPath(workspaceIndexId, rootPath, files, options.signal);
  }

  return { files };
}

export async function scanFilePath(
  workspaceIndexId: string,
  rootPaths: readonly RootPath[],
  absolutePath: string,
  options: ScanOptions = {},
): Promise<ScanResult> {
  const files: FileInfo[] = [];
  for (const rootPath of matchingRootPaths(rootPaths, absolutePath)) {
    throwIfAborted(options.signal);
    const root = normalizeRootPath(rootPath);
    const targetInfo = await lstat(absolutePath).catch(() => null);
    const followedInfo =
      targetInfo?.isSymbolicLink() && root.follow
        ? await stat(absolutePath).catch(() => null)
        : targetInfo;
    if (!followedInfo?.isFile()) {
      continue;
    }
    if (!root.recursive && dirname(absolutePath) !== root.absolutePath) {
      continue;
    }
    const relativePath = toDisplayPath(
      relative(root.absolutePath, absolutePath),
    );
    const rules = await ignoreRulesForDirectory(root, dirname(absolutePath));
    const fileTypes = await resolveFileTypePatterns(
      root.fileTypes,
      root.excludedFileTypes,
    );
    if (
      !pathCanBeScanned(
        root,
        relativePath,
        basename(absolutePath),
        false,
        rules,
      ) ||
      !matchesFileSelection(relativePath, root, fileTypes) ||
      (await hasExcludedNestedGitAncestor(root, absolutePath, false))
    ) {
      continue;
    }
    const file = await readFileInfo(workspaceIndexId, root, absolutePath);
    if (file) {
      files.push(file);
    }
  }
  return { files: dedupeFiles(files) };
}

export async function scanDirectoryPath(
  workspaceIndexId: string,
  rootPaths: readonly RootPath[],
  absolutePath: string,
  options: ScanOptions = {},
): Promise<ScanResult> {
  const files: FileInfo[] = [];
  for (const rootPath of matchingRootPaths(rootPaths, absolutePath)) {
    throwIfAborted(options.signal);
    const root = normalizeRootPath(rootPath);
    if (!root.recursive && absolutePath !== root.absolutePath) {
      continue;
    }
    const info = await lstat(absolutePath).catch(() => null);
    const followedInfo =
      info?.isSymbolicLink() && root.follow
        ? await stat(absolutePath).catch(() => null)
        : info;
    if (!followedInfo?.isDirectory()) {
      continue;
    }
    const relativePath = toDisplayPath(
      relative(root.absolutePath, absolutePath),
    );
    const parentRules = await ignoreRulesForDirectory(
      root,
      dirname(absolutePath),
    );
    const fileTypes = await resolveFileTypePatterns(
      root.fileTypes,
      root.excludedFileTypes,
    );
    if (
      relativePath &&
      (!pathCanBeScanned(
        root,
        relativePath,
        basename(absolutePath),
        true,
        parentRules,
      ) ||
        (await hasExcludedNestedGitAncestor(root, absolutePath, true)))
    ) {
      continue;
    }
    const rootRealPath = await realpath(root.absolutePath).catch(
      () => root.absolutePath,
    );
    const directoryRealPath = await realpath(absolutePath).catch(
      () => absolutePath,
    );
    const depth = relativePath.split("/").filter(Boolean).length;
    await walk(
      workspaceIndexId,
      root,
      absolutePath,
      files,
      parentRules,
      fileTypes,
      new Set([rootRealPath, directoryRealPath]),
      depth,
      options.signal,
    );
  }
  return { files: dedupeFiles(files) };
}

async function hasExcludedNestedGitAncestor(
  rootPath: RootPath,
  absolutePath: string,
  includeTarget: boolean,
): Promise<boolean> {
  const pathFromRoot = relative(rootPath.absolutePath, absolutePath);
  const segments = pathFromRoot.split(sep).filter(Boolean);
  const directorySegments = includeTarget ? segments : segments.slice(0, -1);
  let current = rootPath.absolutePath;
  for (const segment of directorySegments) {
    current = join(current, segment);
    const relativeDirectory = toDisplayPath(
      relative(rootPath.absolutePath, current),
    );
    if (
      (await isNestedGitRepositoryDirectory(current)) &&
      !nestedGitRepositoryExplicitlyIncluded(relativeDirectory, rootPath)
    ) {
      return true;
    }
  }
  return false;
}

function matchingRootPaths(
  rootPaths: readonly RootPath[],
  absolutePath: string,
): RootPath[] {
  return validateRootPaths(rootPaths).filter((rootPath) => {
    const root = normalizeRootPath(rootPath);
    const pathFromRoot = relative(root.absolutePath, absolutePath);
    return (
      !isAbsolute(pathFromRoot) &&
      pathFromRoot !== ".." &&
      !pathFromRoot.startsWith(`..${sep}`)
    );
  });
}

async function ignoreRulesForDirectory(
  rootPath: RootPath,
  directory: string,
): Promise<IgnoreRule[]> {
  const pathFromRoot = relative(rootPath.absolutePath, directory);
  if (
    isAbsolute(pathFromRoot) ||
    pathFromRoot === ".." ||
    pathFromRoot.startsWith(`..${sep}`)
  ) {
    return [...DEFAULT_IGNORE_RULES];
  }
  const rules = [
    ...(rootPath.noIgnore ? [] : DEFAULT_IGNORE_RULES),
    ...(await readConfiguredIgnoreRules(rootPath)),
  ];
  let current = rootPath.absolutePath;
  if (!rootPath.noIgnore) {
    rules.push(...(await readGitIgnoreRules(rootPath, current)));
  }
  if (!pathFromRoot) {
    return rules;
  }
  for (const segment of pathFromRoot.split(sep).filter(Boolean)) {
    current = join(current, segment);
    if (!rootPath.noIgnore) {
      rules.push(...(await readGitIgnoreRules(rootPath, current)));
    }
  }
  return rules;
}

function pathCanBeScanned(
  rootPath: RootPath,
  relativePath: string,
  name: string,
  isDirectory: boolean,
  ignoreRules: readonly IgnoreRule[],
): boolean {
  if (
    relativePath
      .split("/")
      .some((segment) => HARD_SKIP_HIDDEN_NAMES.has(segment))
  ) {
    return false;
  }
  const ignoreMatch = matchIgnoreRules(relativePath, isDirectory, ignoreRules);
  if (
    ignoreMatch.ignored &&
    !ignoredPathExplicitlyIncluded(relativePath, rootPath, ignoreMatch)
  ) {
    return false;
  }
  if (matchesRootExcludePatterns(relativePath, rootPath)) {
    return false;
  }
  if (isDirectory) {
    return !shouldSkipHiddenDirectory(name, relativePath, rootPath);
  }
  return (
    !shouldSkipHiddenFile(name, relativePath, rootPath) &&
    matchesRootPatterns(relativePath, rootPath)
  );
}

function dedupeFiles(files: readonly FileInfo[]): FileInfo[] {
  return [...new Map(files.map((file) => [file.id, file])).values()];
}

async function scanRootPath(
  workspaceIndexId: string,
  rootPath: RootPath,
  files: FileInfo[],
  signal?: AbortSignal,
): Promise<void> {
  throwIfAborted(signal);
  const root = normalizeRootPath(rootPath);
  const fileTypes = await resolveFileTypePatterns(
    root.fileTypes,
    root.excludedFileTypes,
  );
  const info = await stat(root.absolutePath).catch(() => null);

  if (!info) {
    return;
  }

  if (info.isFile()) {
    const relativePath = basename(root.absolutePath);
    const file = matchesFileSelection(relativePath, root, fileTypes)
      ? await readFileInfo(workspaceIndexId, root, root.absolutePath)
      : null;
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

  const rootRealPath = await realpath(root.absolutePath).catch(
    () => root.absolutePath,
  );
  await walk(
    workspaceIndexId,
    root,
    root.absolutePath,
    files,
    [
      ...(root.noIgnore ? [] : DEFAULT_IGNORE_RULES),
      ...(await readConfiguredIgnoreRules(root)),
    ],
    fileTypes,
    new Set([rootRealPath]),
    0,
    signal,
  );
}

async function walk(
  workspaceIndexId: string,
  rootPath: RootPath,
  currentPath: string,
  files: FileInfo[],
  parentIgnoreRules: readonly IgnoreRule[],
  fileTypes: FileTypePatterns,
  visitedDirectories: Set<string>,
  depth: number,
  signal?: AbortSignal,
): Promise<void> {
  throwIfAborted(signal);
  let entries;
  const ignoreRules = [
    ...parentIgnoreRules,
    ...(rootPath.noIgnore
      ? []
      : await readGitIgnoreRules(rootPath, currentPath)),
  ];

  try {
    entries = await readdir(currentPath, { withFileTypes: true });
  } catch {
    return;
  }

  for (const entry of entries) {
    throwIfAborted(signal);
    const absolutePath = join(currentPath, entry.name);
    const relativePath = toDisplayPath(
      relative(rootPath.absolutePath, absolutePath),
    );

    let isDirectory = entry.isDirectory();
    let isFile = entry.isFile();
    if (entry.isSymbolicLink() && rootPath.follow) {
      const target = await stat(absolutePath).catch(() => null);
      isDirectory = target?.isDirectory() === true;
      isFile = target?.isFile() === true;
    }

    if (isDirectory) {
      if (!rootPath.recursive) {
        continue;
      }
      if (rootPath.maxDepth !== undefined && depth + 1 >= rootPath.maxDepth) {
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

      const realDirectory = await realpath(absolutePath).catch(() => null);
      if (!realDirectory || visitedDirectories.has(realDirectory)) {
        continue;
      }
      visitedDirectories.add(realDirectory);
      await walk(
        workspaceIndexId,
        rootPath,
        absolutePath,
        files,
        ignoreRules,
        fileTypes,
        visitedDirectories,
        depth + 1,
        signal,
      );
      continue;
    }

    if (!isFile) {
      continue;
    }

    if (rootPath.maxDepth !== undefined && depth + 1 > rootPath.maxDepth) {
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

    if (!matchesFileSelection(relativePath, rootPath, fileTypes)) {
      continue;
    }

    const file = await readFileInfo(workspaceIndexId, rootPath, absolutePath);
    throwIfAborted(signal);
    if (file) {
      files.push(file);
    }
  }
}

function throwIfAborted(signal: AbortSignal | undefined): void {
  if (!signal?.aborted) {
    return;
  }
  throw signal.reason instanceof Error
    ? signal.reason
    : new Error("Indexing was cancelled.");
}

function isHiddenName(name: string): boolean {
  return name.startsWith(".") && name !== "." && name !== "..";
}

async function readGitIgnoreRules(
  rootPath: RootPath,
  currentPath: string,
): Promise<IgnoreRule[]> {
  const ignorePath = join(currentPath, ".gitignore");
  const content = await readFile(ignorePath, "utf8").catch(() => null);
  const basePath = toDisplayPath(relative(rootPath.absolutePath, currentPath));
  const cacheKey = `${ignorePath}\0${basePath}`;
  if (content === null) {
    gitIgnoreRuleCache.delete(cacheKey);
    return [];
  }

  const cached = gitIgnoreRuleCache.get(cacheKey);
  if (cached?.content === content) {
    return cached.rules;
  }
  const rules = parseGitIgnoreRules(content, basePath);
  gitIgnoreRuleCache.set(cacheKey, { content, rules });
  if (gitIgnoreRuleCache.size > MAX_GITIGNORE_CACHE_ENTRIES) {
    gitIgnoreRuleCache.delete(gitIgnoreRuleCache.keys().next().value!);
  }
  return rules;
}

async function readConfiguredIgnoreRules(
  rootPath: RootPath,
): Promise<IgnoreRule[]> {
  const rules: IgnoreRule[] = [];
  for (const path of rootPath.ignoreFiles ?? []) {
    const absolutePath = isAbsolute(path)
      ? path
      : resolve(rootPath.absolutePath, path);
    const content = await readFile(absolutePath, "utf8");
    rules.push(...parseGitIgnoreRules(content, ""));
  }
  return rules;
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

  return (
    segmentMatches(segment, ignoredSegment) ||
    segmentMatches(ignoredSegment, segment)
  );
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
  if (!isHiddenName(name) || rootPath.hidden) {
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
    !rootPath.hidden &&
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
  workspaceIndexId: string,
  rootPath: RootPath,
  absolutePath: string,
): Promise<FileInfo | null> {
  const info = await stat(absolutePath).catch(() => null);

  if (!info || !info.isFile()) {
    return null;
  }

  const maxFileSize = rootPath.maxFileSizeBytes ?? DEFAULT_MAX_FILE_SIZE_BYTES;
  if (info.size === 0 || info.size > maxFileSize) {
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
    id: makeFileId(workspaceIndexId, absolutePath),
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

function makeFileId(workspaceIndexId: string, absolutePath: string): string {
  return sha256Text(`${workspaceIndexId}\0${normalizePath(absolutePath)}`);
}
