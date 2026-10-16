import { lstat, open, readdir, realpath, stat } from "node:fs/promises";
import { setImmediate as yieldToEventLoop } from "node:timers/promises";
import {
  chargeGlobOverhead,
  checkActiveRuleWeight,
  checkGlobCancellation,
  checkGlobRuleCount,
  labeledGlobError,
  globWorkNeedsYield,
  withGlobBudget,
  withGlobOverheadOnly,
  withGlobPathBudget,
  yieldGlobWorkIfNeeded,
} from "../../../utils/glob-budget.js";
import { compileGlob } from "../../../utils/glob-matcher.js";
import {
  basename,
  dirname,
  isAbsolute,
  join,
  relative,
  resolve,
  sep,
} from "node:path";
import type {
  FileInfo,
  FileScanDiagnostics,
  RootPath,
  SkippedFile,
} from "../../../types.js";
import { detectFileType } from "../../../file-type.js";
import { resolveMaxFileSizeBytes } from "../../../file-size-policy.js";
import { sha256Text } from "../../../utils/hash.js";
import {
  matchesFileSelection,
  resolveFileTypePatterns,
  type FileSelection,
  type FileTypePatterns,
} from "../../../utils/file-selection.js";
import {
  globPatternWeight,
  hasPathGlob,
  normalizePathForMatch,
  normalizePathPattern,
  pathPatternMatches,
  pathPatternMatchesPrepared,
  pathPatternMightMatchDescendant,
  ripgrepPatternWeight,
} from "../../../utils/glob.js";
import { normalizePath, toDisplayPath } from "../../../utils/path.js";
import {
  matchesRootExcludePatterns,
  matchesRootPatterns,
  normalizeRootPath,
  validateRootPaths,
} from "../root-paths.js";

const BINARY_SNIFF_BYTES = 8192;
const BINARY_CONTROL_CHAR_RATIO = 0.3;
const MAX_GITIGNORE_CACHE_ENTRIES = 4_096;
const MAX_IGNORE_FILE_BYTES = 1_048_576;
const MAX_GITIGNORE_CACHE_CHARS = 8_388_608;
let gitIgnoreCacheChars = 0;

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
  source: string;
  line: number;
};

type IgnoreMatch = {
  ignored: boolean;
  matchedNegation: boolean;
  matchedRule?: IgnoreRule;
};

const BUILT_IN_IGNORE_RULE_SOURCE = "built-in ignore rules";

const DEFAULT_IGNORE_RULES: readonly IgnoreRule[] = [
  ...DEFAULT_IGNORED_DIRECTORY_NAMES.map((pattern) => ({
    basePath: "",
    pattern,
    negated: false,
    directoryOnly: true,
    anchored: false,
    hasSlash: false,
    source: BUILT_IN_IGNORE_RULE_SOURCE,
    line: 0,
  })),
  ...DEFAULT_IGNORED_FILE_PATTERNS.map((pattern) => ({
    basePath: "",
    pattern,
    negated: false,
    directoryOnly: false,
    anchored: false,
    hasSlash: false,
    source: BUILT_IN_IGNORE_RULE_SOURCE,
    line: 0,
  })),
];

const gitIgnoreRuleCache = new Map<
  string,
  { content: string; rules: IgnoreRule[] }
>();

export type ScanResult = {
  files: FileInfo[];
  diagnostics: FileScanDiagnostics;
};

export type ScanOptions = {
  signal?: AbortSignal;
  knownFiles?: readonly FileInfo[];
};

function knownFilesByPath(files: readonly FileInfo[] | undefined) {
  return new Map(
    (files ?? []).map((file) => [normalizePath(file.absolutePath), file]),
  );
}

export async function scanRootPaths(
  workspaceIndexId: string,
  rootPaths: readonly RootPath[],
  options: ScanOptions = {},
): Promise<ScanResult> {
  return withGlobBudget(() =>
    scanRootPathsImpl(workspaceIndexId, rootPaths, options),
  );
}

async function scanRootPathsImpl(
  workspaceIndexId: string,
  rootPaths: readonly RootPath[],
  options: ScanOptions = {},
): Promise<ScanResult> {
  const validatedRootPaths = validateRootPaths(rootPaths);
  const files: FileInfo[] = [];
  const diagnostics = createScanDiagnostics();
  const knownFiles = knownFilesByPath(options.knownFiles);

  for (const rootPath of validatedRootPaths) {
    throwIfAborted(options.signal);
    await scanRootPath(
      workspaceIndexId,
      rootPath,
      files,
      diagnostics,
      options.signal,
      knownFiles,
    );
  }

  return { files, diagnostics };
}

export async function scanFilePath(
  workspaceIndexId: string,
  rootPaths: readonly RootPath[],
  absolutePath: string,
  options: ScanOptions = {},
): Promise<ScanResult> {
  return withGlobBudget(() =>
    scanFilePathImpl(workspaceIndexId, rootPaths, absolutePath, options),
  );
}

async function scanFilePathImpl(
  workspaceIndexId: string,
  rootPaths: readonly RootPath[],
  absolutePath: string,
  options: ScanOptions = {},
): Promise<ScanResult> {
  const files: FileInfo[] = [];
  const diagnostics = createScanDiagnostics();
  const knownFiles = knownFilesByPath(options.knownFiles);
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
    const acceptable = await withGlobPathBudget(
      relativePath.length,
      admitScanRules(
        rootFilterWeight(root, fileTypes) + ignoreRulesWeight(rules),
        root,
      ),
      async () =>
        (await pathCanBeScanned(
          root,
          relativePath,
          basename(absolutePath),
          false,
          rules,
          options.signal,
        )) &&
        matchesFileSelection(relativePath, root, fileTypes, {
          signal: options.signal,
        }),
    );
    if (
      !acceptable ||
      (await hasExcludedNestedGitAncestor(root, absolutePath, false))
    ) {
      continue;
    }
    const file = await readFileInfo(
      workspaceIndexId,
      root,
      absolutePath,
      diagnostics,
      knownFiles,
    );
    if (file) {
      files.push(file);
    }
  }
  return { files: dedupeFiles(files), diagnostics };
}

export async function pathCanAffectIndex(
  rootPaths: readonly RootPath[],
  absolutePath: string,
  isDirectory: boolean,
  signal?: AbortSignal,
): Promise<boolean> {
  return withGlobBudget(() =>
    pathCanAffectIndexImpl(rootPaths, absolutePath, isDirectory, signal),
  );
}

async function pathCanAffectIndexImpl(
  rootPaths: readonly RootPath[],
  absolutePath: string,
  isDirectory: boolean,
  signal?: AbortSignal,
): Promise<boolean> {
  for (const configuredRoot of rootPaths) {
    const root = normalizeRootPath(configuredRoot);
    const pathFromRoot = relative(root.absolutePath, absolutePath);
    if (
      isAbsolute(pathFromRoot) ||
      pathFromRoot === ".." ||
      pathFromRoot.startsWith(`..${sep}`)
    ) {
      continue;
    }
    if (pathFromRoot.length === 0) {
      return true;
    }

    const depth = pathFromRoot.split(sep).filter(Boolean).length;
    if (
      (!root.recursive &&
        (isDirectory || dirname(absolutePath) !== root.absolutePath)) ||
      (root.maxDepth !== undefined &&
        (isDirectory ? depth >= root.maxDepth : depth > root.maxDepth))
    ) {
      continue;
    }

    const relativePath = toDisplayPath(pathFromRoot);
    const rules = await ignoreRulesForDirectory(root, dirname(absolutePath));
    const pathFileTypes = await resolveFileTypePatterns(
      root.fileTypes,
      root.excludedFileTypes,
    );
    const acceptable = await withGlobPathBudget(
      relativePath.length,
      admitScanRules(
        rootFilterWeight(root, pathFileTypes) + ignoreRulesWeight(rules),
        root,
      ),
      async () =>
        (await pathCanBeScanned(
          root,
          relativePath,
          basename(absolutePath),
          isDirectory,
          rules,
          signal,
        )) &&
        (isDirectory ||
          matchesFileSelection(relativePath, root, pathFileTypes, {
            signal,
          })),
    );
    if (
      !acceptable ||
      (await hasExcludedNestedGitAncestor(root, absolutePath, isDirectory))
    ) {
      continue;
    }
    return true;
  }
  return false;
}

export async function scanDirectoryPath(
  workspaceIndexId: string,
  rootPaths: readonly RootPath[],
  absolutePath: string,
  options: ScanOptions = {},
): Promise<ScanResult> {
  return withGlobBudget(() =>
    scanDirectoryPathImpl(workspaceIndexId, rootPaths, absolutePath, options),
  );
}

async function scanDirectoryPathImpl(
  workspaceIndexId: string,
  rootPaths: readonly RootPath[],
  absolutePath: string,
  options: ScanOptions = {},
): Promise<ScanResult> {
  const files: FileInfo[] = [];
  const diagnostics = createScanDiagnostics();
  const knownFiles = knownFilesByPath(options.knownFiles);
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
    const staticRuleWeight = admitScanRules(
      rootFilterWeight(root, fileTypes),
      root,
    );
    const directoryAcceptable =
      !relativePath ||
      (await withGlobPathBudget(
        relativePath.length,
        admitScanRules(staticRuleWeight + ignoreRulesWeight(parentRules), root),
        async () =>
          pathCanBeScanned(
            root,
            relativePath,
            basename(absolutePath),
            true,
            parentRules,
            options.signal,
          ),
      ));
    if (
      !directoryAcceptable ||
      (await hasExcludedNestedGitAncestor(root, absolutePath, true))
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
      diagnostics,
      parentRules,
      fileTypes,
      staticRuleWeight,
      new Set([rootRealPath, directoryRealPath]),
      depth,
      options.signal,
      knownFiles,
    );
  }
  return { files: dedupeFiles(files), diagnostics };
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
    await yieldGlobWorkIfNeeded(yieldToEventLoop);
    if (
      (await isNestedGitRepositoryDirectory(current)) &&
      !(await nestedGitRepositoryExplicitlyIncluded(
        relativeDirectory,
        rootPath,
      ))
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

async function pathCanBeScanned(
  rootPath: RootPath,
  relativePath: string,
  name: string,
  isDirectory: boolean,
  ignoreRules: readonly IgnoreRule[],
  signal?: AbortSignal,
): Promise<boolean> {
  if (
    relativePath
      .split("/")
      .some((segment) => HARD_SKIP_HIDDEN_NAMES.has(segment))
  ) {
    return false;
  }
  const ignoreMatch = await matchIgnoreRules(
    relativePath,
    isDirectory,
    ignoreRules,
    signal,
  );
  if (
    ignoreMatch.ignored &&
    !(await ignoredPathExplicitlyIncluded(
      relativePath,
      rootPath,
      ignoreMatch,
      signal,
    ))
  ) {
    return false;
  }
  if (await matchesRootExcludePatterns(relativePath, rootPath, signal)) {
    return false;
  }
  if (isDirectory) {
    return !(await shouldSkipHiddenDirectory(
      name,
      relativePath,
      rootPath,
      signal,
    ));
  }
  return (
    !(await shouldSkipHiddenFile(name, relativePath, rootPath, signal)) &&
    (await matchesRootPatterns(relativePath, rootPath, signal))
  );
}

function dedupeFiles(files: readonly FileInfo[]): FileInfo[] {
  return [...new Map(files.map((file) => [file.id, file])).values()];
}

async function scanRootPath(
  workspaceIndexId: string,
  rootPath: RootPath,
  files: FileInfo[],
  diagnostics: FileScanDiagnostics,
  signal?: AbortSignal,
  knownFiles: ReadonlyMap<string, FileInfo> = new Map(),
): Promise<void> {
  throwIfAborted(signal);
  const root = normalizeRootPath(rootPath);
  const fileTypes = await resolveFileTypePatterns(
    root.fileTypes,
    root.excludedFileTypes,
  );
  const staticRuleWeight = admitScanRules(
    rootFilterWeight(root, fileTypes),
    root,
  );
  const info = await stat(root.absolutePath).catch(() => null);

  if (!info) {
    return;
  }

  if (info.isFile()) {
    const relativePath = basename(root.absolutePath);
    const keepRootFile = await withGlobPathBudget(
      relativePath.length,
      staticRuleWeight,
      async () => matchesFileSelection(relativePath, root, fileTypes),
    );
    const file = keepRootFile
      ? await readFileInfo(
          workspaceIndexId,
          root,
          root.absolutePath,
          diagnostics,
          knownFiles,
        )
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
    diagnostics,
    [
      ...(root.noIgnore ? [] : DEFAULT_IGNORE_RULES),
      ...(await readConfiguredIgnoreRules(root)),
    ],
    fileTypes,
    staticRuleWeight,
    new Set([rootRealPath]),
    0,
    signal,
    knownFiles,
  );
}

function admitScanRules(weight: number, rootPath: RootPath): number {
  checkActiveRuleWeight(
    weight,
    `active ignore and filter rules for ${rootPath.absolutePath}`,
  );
  return weight;
}

function ignoreRulesWeight(rules: readonly IgnoreRule[]): number {
  let weight = 0;
  for (const rule of rules) {
    weight += globPatternWeight(rule.pattern, ignoreRuleLabel(rule));
  }
  return weight;
}

function ignoreRuleLabel(rule: IgnoreRule): string {
  return `ignore rule ${rule.line > 0 ? `${rule.source}:${rule.line}` : rule.source}`;
}

function fileTypesWeight(fileTypes: FileTypePatterns): number {
  let weight = 0;
  for (const entry of fileTypes.include) {
    weight += ripgrepPatternWeight(entry.pattern, false, entry.origin);
  }
  for (const entry of fileTypes.exclude) {
    weight += ripgrepPatternWeight(entry.pattern, false, entry.origin);
  }
  return weight;
}

function selectionWeight(selection: FileSelection): number {
  let weight = 0;
  for (const [index, pattern] of (selection.globs ?? []).entries()) {
    weight += ripgrepPatternWeight(pattern, false, `globs[${index}]`);
  }
  for (const [index, pattern] of (selection.insensitiveGlobs ?? []).entries()) {
    weight += ripgrepPatternWeight(pattern, true, `insensitiveGlobs[${index}]`);
  }
  return weight;
}

function rootFilterWeight(
  rootPath: RootPath,
  fileTypes: FileTypePatterns,
): number {
  let weight = selectionWeight(rootPath);
  for (const [index, pattern] of (rootPath.include ?? []).entries()) {
    weight += globPatternWeight(pattern, `root include[${index}]`);
  }
  for (const [index, pattern] of (rootPath.exclude ?? []).entries()) {
    weight += globPatternWeight(pattern, `root exclude[${index}]`);
  }
  return weight + fileTypesWeight(fileTypes);
}

async function walk(
  workspaceIndexId: string,
  rootPath: RootPath,
  currentPath: string,
  files: FileInfo[],
  diagnostics: FileScanDiagnostics,
  parentIgnoreRules: readonly IgnoreRule[],
  fileTypes: FileTypePatterns,
  staticRuleWeight: number,
  visitedDirectories: Set<string>,
  depth: number,
  signal?: AbortSignal,
  knownFiles: ReadonlyMap<string, FileInfo> = new Map(),
): Promise<void> {
  throwIfAborted(signal);
  let entries;
  const ignoreRules = [
    ...parentIgnoreRules,
    ...(rootPath.noIgnore
      ? []
      : await readGitIgnoreRules(rootPath, currentPath)),
  ];
  checkGlobRuleCount(ignoreRules.length);
  const activeWeight = admitScanRules(
    staticRuleWeight + ignoreRulesWeight(ignoreRules),
    rootPath,
  );

  try {
    entries = await readdir(currentPath, { withFileTypes: true });
  } catch {
    return;
  }

  let entriesProcessed = 0;
  for (const entry of entries) {
    if (++entriesProcessed % 128 === 0 || globWorkNeedsYield())
      await yieldToEventLoop();
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

      const { skipDirectory } = await withGlobPathBudget(
        relativePath.length,
        activeWeight,
        async () => {
          const normalizedPath = normalizePathForMatch(relativePath);
          const ignoreMatch = await matchIgnoreRules(
            normalizedPath,
            true,
            ignoreRules,
            signal,
          );
          return {
            skipDirectory:
              HARD_SKIP_HIDDEN_NAMES.has(entry.name) ||
              (await matchesRootExcludePatterns(
                relativePath,
                rootPath,
                signal,
              )) ||
              (ignoreMatch.ignored &&
                !(await ignoredPathExplicitlyIncluded(
                  normalizedPath,
                  rootPath,
                  ignoreMatch,
                  signal,
                ))) ||
              (await shouldSkipHiddenDirectory(
                entry.name,
                normalizedPath,
                rootPath,
                signal,
              )),
          };
        },
      );
      if (skipDirectory) {
        continue;
      }

      if (
        (await isNestedGitRepositoryDirectory(absolutePath)) &&
        !(await nestedGitRepositoryExplicitlyIncluded(
          relativePath,
          rootPath,
          signal,
        ))
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
        diagnostics,
        ignoreRules,
        fileTypes,
        staticRuleWeight,
        visitedDirectories,
        depth + 1,
        signal,
        knownFiles,
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

    const keepFile = await withGlobPathBudget(
      relativePath.length,
      activeWeight,
      async () => {
        const normalizedPath = normalizePathForMatch(relativePath);
        const ignoreMatch = await matchIgnoreRules(
          normalizedPath,
          false,
          ignoreRules,
          signal,
        );
        if (
          ignoreMatch.ignored &&
          !(await ignoredPathExplicitlyIncluded(
            normalizedPath,
            rootPath,
            ignoreMatch,
            signal,
          ))
        ) {
          return false;
        }

        if (
          await shouldSkipHiddenFile(
            entry.name,
            normalizedPath,
            rootPath,
            signal,
          )
        ) {
          return false;
        }

        if (!(await matchesRootPatterns(normalizedPath, rootPath, signal))) {
          return false;
        }

        return matchesFileSelection(normalizedPath, rootPath, fileTypes, {
          signal,
          yieldToEventLoop,
        });
      },
    );
    if (!keepFile) {
      continue;
    }

    const file = await readFileInfo(
      workspaceIndexId,
      rootPath,
      absolutePath,
      diagnostics,
      knownFiles,
    );
    throwIfAborted(signal);
    if (file) {
      files.push(file);
    }
  }
}

function throwIfAborted(signal: AbortSignal | undefined): void {
  checkGlobCancellation(signal);
}

function isHiddenName(name: string): boolean {
  return name.startsWith(".") && name !== "." && name !== "..";
}

async function readGitIgnoreRules(
  rootPath: RootPath,
  currentPath: string,
): Promise<IgnoreRule[]> {
  const ignorePath = join(currentPath, ".gitignore");
  const content = await readIgnoreFile(ignorePath, true);
  const basePath = toDisplayPath(relative(rootPath.absolutePath, currentPath));
  const cacheKey = `${ignorePath}\0${basePath}`;
  if (content === null) {
    gitIgnoreCacheChars -=
      gitIgnoreRuleCache.get(cacheKey)?.content.length ?? 0;
    gitIgnoreRuleCache.delete(cacheKey);
    return [];
  }

  const cached = gitIgnoreRuleCache.get(cacheKey);
  if (cached?.content === content) {
    return cached.rules;
  }
  const rules = parseGitIgnoreRules(content, basePath, ignorePath);
  gitIgnoreCacheChars -= cached?.content.length ?? 0;
  gitIgnoreCacheChars += content.length;
  gitIgnoreRuleCache.set(cacheKey, { content, rules });
  while (
    gitIgnoreRuleCache.size > MAX_GITIGNORE_CACHE_ENTRIES ||
    gitIgnoreCacheChars > MAX_GITIGNORE_CACHE_CHARS
  ) {
    const oldest = gitIgnoreRuleCache.keys().next().value!;
    gitIgnoreCacheChars -= gitIgnoreRuleCache.get(oldest)!.content.length;
    gitIgnoreRuleCache.delete(oldest);
  }
  return rules;
}

async function readConfiguredIgnoreRules(
  rootPath: RootPath,
): Promise<IgnoreRule[]> {
  const rules: IgnoreRule[] = [];
  checkGlobRuleCount(rootPath.ignoreFiles?.length ?? 0);
  for (const path of rootPath.ignoreFiles ?? []) {
    const absolutePath = isAbsolute(path)
      ? path
      : resolve(rootPath.absolutePath, path);
    const content = await readIgnoreFile(absolutePath, false);
    rules.push(...parseGitIgnoreRules(content!, "", absolutePath));
    checkGlobRuleCount(rules.length);
  }
  return rules;
}

async function readIgnoreFile(
  path: string,
  optional: boolean,
): Promise<string | null> {
  const handle = await open(path, "r").catch((error: unknown) => {
    if (optional && (error as NodeJS.ErrnoException).code === "ENOENT")
      return null;
    throw error;
  });
  if (!handle) return null;
  try {
    const chunks: Buffer[] = [];
    let size = 0;
    for (;;) {
      const buffer = Buffer.alloc(
        Math.min(65_536, MAX_IGNORE_FILE_BYTES + 1 - size),
      );
      const { bytesRead } = await handle.read(buffer);
      if (bytesRead === 0) return Buffer.concat(chunks).toString("utf8");
      size += bytesRead;
      chargeGlobOverhead(bytesRead);
      if (size > MAX_IGNORE_FILE_BYTES)
        throw new Error(
          `Ignore file exceeds the ${MAX_IGNORE_FILE_BYTES}-byte limit: ${path}`,
        );
      chunks.push(buffer.subarray(0, bytesRead));
    }
  } finally {
    await handle.close();
  }
}

function parseGitIgnoreRules(
  content: string,
  basePath: string,
  source: string,
): IgnoreRule[] {
  const rules: IgnoreRule[] = [];

  for (const [line, rawLine] of content.split(/\r?\n/).entries()) {
    try {
      const rule = parseGitIgnoreRule(rawLine, basePath, source, line + 1);
      if (rule) rules.push(rule);
      checkGlobRuleCount(rules.length);
    } catch (error) {
      throw new Error(
        `Invalid ignore rule at ${source}:${line + 1}: ${error instanceof Error ? error.message : String(error)}`,
        { cause: error },
      );
    }
  }

  return rules;
}

function parseGitIgnoreRule(
  line: string,
  basePath: string,
  source: string,
  lineNumber: number,
): IgnoreRule | null {
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
  pattern = withGlobOverheadOnly(() => normalizePathPattern(pattern));

  if (pattern.length === 0) {
    return null;
  }

  // Validate before scanning any files, preserving the source/line in errors.
  if (hasPathGlob(pattern)) compileGlob(pattern, false);

  return {
    basePath,
    pattern,
    negated,
    directoryOnly,
    anchored,
    hasSlash: pattern.includes("/"),
    source,
    line: lineNumber,
  };
}

async function matchIgnoreRules(
  relativePath: string,
  isDirectory: boolean,
  rules: readonly IgnoreRule[],
  signal?: AbortSignal,
): Promise<IgnoreMatch> {
  checkGlobRuleCount(rules.length);
  let ignored = false;
  let matchedNegation = false;
  let matchedRule: IgnoreRule | undefined;

  for (const rule of rules) {
    await yieldGlobWorkIfNeeded(yieldToEventLoop, signal);
    let matches: boolean;
    try {
      matches = await ignoreRuleMatches(
        rule,
        relativePath,
        isDirectory,
        signal,
      );
    } catch (error) {
      throw ignoreRuleError(rule, error);
    }
    if (!matches) {
      continue;
    }

    ignored = !rule.negated;
    matchedNegation = rule.negated;
    matchedRule = rule;
  }
  await yieldGlobWorkIfNeeded(yieldToEventLoop, signal);

  return {
    ignored,
    matchedNegation,
    matchedRule,
  };
}

function ignoreRuleError(rule: IgnoreRule, cause: unknown): Error {
  const origin = rule.line > 0 ? `${rule.source}:${rule.line}` : rule.source;
  return labeledGlobError(`ignore rule ${origin}`, rule.pattern, cause);
}

async function ignoredPathExplicitlyIncluded(
  relativePath: string,
  rootPath: RootPath,
  ignoreMatch: IgnoreMatch,
  signal?: AbortSignal,
): Promise<boolean> {
  if (!ignoreMatch.ignored || !ignoreMatch.matchedRule || !rootPath.include) {
    return false;
  }

  for (const [includeIndex, pattern] of rootPath.include.entries()) {
    await yieldGlobWorkIfNeeded(yieldToEventLoop, signal);
    try {
      if (
        includePatternNamesIgnoredPath(
          pattern,
          relativePath,
          ignoreMatch.matchedRule!.pattern,
        )
      )
        return true;
    } catch (error) {
      throw labeledGlobError(`root include[${includeIndex}]`, pattern, error);
    }
  }
  return false;
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

async function ignoreRuleMatches(
  rule: IgnoreRule,
  relativePath: string,
  isDirectory: boolean,
  signal?: AbortSignal,
): Promise<boolean> {
  const path = relativeToIgnoreRuleBase(relativePath, rule.basePath);
  if (path === null || path.length === 0) {
    return false;
  }

  if (rule.directoryOnly) {
    if (rule.anchored || rule.hasSlash) {
      return pathPatternMatchesPrepared(rule.pattern, path);
    }

    return pathContainsMatchingSegment(path, rule.pattern, signal);
  }

  if (rule.anchored || rule.hasSlash) {
    return pathPatternMatchesPrepared(rule.pattern, path);
  }

  if (
    isDirectory &&
    (await pathContainsMatchingSegment(path, rule.pattern, signal))
  ) {
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

async function pathContainsMatchingSegment(
  path: string,
  pattern: string,
  signal?: AbortSignal,
): Promise<boolean> {
  for (const segment of path.split("/")) {
    await yieldGlobWorkIfNeeded(yieldToEventLoop, signal);
    if (segmentMatches(pattern, segment)) {
      return true;
    }
  }
  return false;
}

function segmentMatches(pattern: string, segment: string): boolean {
  // Equivalent fast paths, proven against the compiled matcher's semantics:
  // - a pattern without glob syntax (including braces, which the matcher
  //   treats as alternation) matches a path segment — which never contains
  //   "/" — only by exact equality;
  // - "*<literal>" with a brace-free, metacharacter-free suffix matches a
  //   segment iff it ends with that literal suffix, because a single "*"
  //   spans only non-separator characters.
  // Every other pattern keeps the compiled matcher.
  if (isLiteralGlobText(pattern)) {
    return pattern === segment;
  }
  if (pattern.startsWith("*") && isLiteralGlobText(pattern.slice(1))) {
    return segment.endsWith(pattern.slice(1));
  }
  return pathPatternMatchesPrepared(pattern, segment);
}

function isLiteralGlobText(value: string): boolean {
  return (
    !value.includes("*") &&
    !value.includes("?") &&
    !value.includes("[") &&
    !value.includes("{")
  );
}

async function shouldSkipHiddenDirectory(
  name: string,
  relativePath: string,
  rootPath: RootPath,
  signal?: AbortSignal,
): Promise<boolean> {
  if (!isHiddenName(name) || rootPath.hidden) {
    return false;
  }

  return !(await hasIncludeDescendant(relativePath, rootPath.include, signal));
}

async function shouldSkipHiddenFile(
  name: string,
  relativePath: string,
  rootPath: RootPath,
  signal?: AbortSignal,
): Promise<boolean> {
  return (
    isHiddenName(name) &&
    !rootPath.hidden &&
    !(await hasExplicitHiddenFileInclude(
      relativePath,
      rootPath.include,
      signal,
    ))
  );
}

async function isNestedGitRepositoryDirectory(
  absolutePath: string,
): Promise<boolean> {
  const marker = await stat(join(absolutePath, ".git")).catch(() => null);
  return marker !== null && (marker.isFile() || marker.isDirectory());
}

async function nestedGitRepositoryExplicitlyIncluded(
  relativePath: string,
  rootPath: RootPath,
  signal?: AbortSignal,
): Promise<boolean> {
  if (!rootPath.include || rootPath.include.length === 0) {
    return false;
  }

  const normalizedRelativePath = normalizePathPattern(relativePath);
  for (const [includeIndex, pattern] of rootPath.include.entries()) {
    await yieldGlobWorkIfNeeded(yieldToEventLoop, signal);
    const normalizedPattern = normalizePathPattern(pattern);
    try {
      if (
        pathPatternMatches(normalizedPattern, normalizedRelativePath) ||
        normalizedPattern.startsWith(`${normalizedRelativePath}/`)
      ) {
        return true;
      }
    } catch (error) {
      throw labeledGlobError(`root include[${includeIndex}]`, pattern, error);
    }
  }
  return false;
}

async function hasIncludeDescendant(
  relativePath: string,
  includePatterns: readonly string[] | undefined,
  signal?: AbortSignal,
): Promise<boolean> {
  if (!includePatterns || includePatterns.length === 0) {
    return false;
  }

  for (const pattern of includePatterns) {
    await yieldGlobWorkIfNeeded(yieldToEventLoop, signal);
    if (
      includePatternDeclaresHiddenDirectory(pattern, relativePath) &&
      pathPatternMightMatchDescendant(pattern, relativePath)
    ) {
      return true;
    }
  }
  return false;
}

async function hasExplicitHiddenFileInclude(
  relativePath: string,
  includePatterns: readonly string[] | undefined,
  signal?: AbortSignal,
): Promise<boolean> {
  if (!includePatterns || includePatterns.length === 0) {
    return false;
  }

  for (const pattern of includePatterns) {
    await yieldGlobWorkIfNeeded(yieldToEventLoop, signal);
    if (
      includePatternDeclaresHiddenDirectory(pattern, relativePath) &&
      pathPatternMatches(pattern, relativePath)
    ) {
      return true;
    }
  }
  return false;
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
  diagnostics: FileScanDiagnostics,
  knownFiles: ReadonlyMap<string, FileInfo> = new Map(),
): Promise<FileInfo | null> {
  const info = await stat(absolutePath).catch(() => null);

  if (!info || !info.isFile()) {
    return null;
  }

  const relativePath =
    toDisplayPath(relative(rootPath.absolutePath, absolutePath)) ||
    basename(absolutePath);
  if (info.size === 0) {
    recordSkippedFile(diagnostics, {
      absolutePath,
      relativePath,
      reason: "empty",
      sizeBytes: 0,
    });
    return null;
  }

  const detected = detectFileType(absolutePath);
  if (!detected) {
    recordSkippedFile(diagnostics, {
      absolutePath,
      relativePath,
      reason: "unsupported",
      sizeBytes: info.size,
    });
    return null;
  }

  const maxFileSize = resolveMaxFileSizeBytes(
    detected.kind,
    rootPath.maxFileSizeBytes,
  );
  if (info.size > maxFileSize) {
    recordSkippedFile(diagnostics, {
      absolutePath,
      relativePath,
      reason: "too_large",
      sizeBytes: info.size,
      limitBytes: maxFileSize,
    });
    return null;
  }

  const lastModifiedTime = Math.trunc(info.mtimeMs);
  const known = knownFiles.get(normalizePath(absolutePath));
  if (
    known?.contentHash &&
    known.sizeBytes === info.size &&
    known.lastModifiedTime === lastModifiedTime
  ) {
    return {
      id: makeFileId(workspaceIndexId, absolutePath),
      absolutePath,
      relativePath,
      rootPath: rootPath.absolutePath,
      sizeBytes: info.size,
      lastModifiedTime,
      kind: detected.kind,
      format: detected.format,
    };
  }

  if (detected.kind !== "image" && (await isLikelyBinaryFile(absolutePath))) {
    recordSkippedFile(diagnostics, {
      absolutePath,
      relativePath,
      reason: "binary",
      sizeBytes: info.size,
    });
    return null;
  }

  return {
    id: makeFileId(workspaceIndexId, absolutePath),
    absolutePath,
    relativePath,
    rootPath: rootPath.absolutePath,
    sizeBytes: info.size,
    lastModifiedTime,
    kind: detected.kind,
    format: detected.format,
  };
}

const MAX_SKIPPED_FILE_SAMPLES = 20;

export function createScanDiagnostics(): FileScanDiagnostics {
  return {
    skippedFiles: 0,
    skippedByReason: {
      empty: 0,
      too_large: 0,
      unsupported: 0,
      binary: 0,
    },
    skippedSamples: [],
  };
}

function recordSkippedFile(
  diagnostics: FileScanDiagnostics,
  skipped: SkippedFile,
): void {
  diagnostics.skippedFiles++;
  diagnostics.skippedByReason[skipped.reason]++;
  if (diagnostics.skippedSamples.length < MAX_SKIPPED_FILE_SAMPLES) {
    diagnostics.skippedSamples.push(skipped);
  }
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
