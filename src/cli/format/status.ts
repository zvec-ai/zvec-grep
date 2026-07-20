import type {
  CollectionIndexStatus,
  CollectionInfo,
  IndexResult,
  ZvecGrepInfoResult,
} from "../../index.js";
import type { CliOptions } from "../types.js";
import { shouldUseColor } from "./highlight.js";

type StatusTheme = {
  label(value: string): string;
  path(value: string): string;
  success(value: string): string;
  warning(value: string): string;
  danger(value: string): string;
  accent(value: string): string;
  muted(value: string): string;
};

export function printCollectionList(
  collections: readonly CollectionInfo[],
  options: CliOptions,
): void {
  const theme = createStatusTheme(options);
  for (const collection of collections) {
    const roots = collection.rootPaths.map(formatRootPath).join(", ");
    console.log(`${theme.accent(collection.name)}\t${theme.path(roots)}`);
  }
}

export function printCollectionInfo(
  info: CollectionInfo,
  status: CollectionIndexStatus | null,
  options: CliOptions,
): void {
  const theme = createStatusTheme(options);

  printField(theme, "name", theme.accent(info.name));
  printField(theme, "id", info.id);
  printField(theme, "path", theme.path(info.path));
  printField(theme, "policy", info.indexPolicy ?? "enabled");
  if (info.embedding) {
    printField(
      theme,
      "embedding",
      `${info.embedding.provider}/${info.embedding.model}`,
    );
  } else {
    printField(theme, "embedding", theme.warning("none"));
  }
  printField(
    theme,
    "roots",
    theme.path(info.rootPaths.map(formatRootPath).join(", ")),
  );

  if (status) {
    printIndexStatus(theme, status);
    if (statusNeedsRefresh(status)) {
      printField(
        theme,
        "suggestion",
        theme.accent(`zg collections index ${shellArg(info.name)}`),
      );
    }
    printFailedFilesNote(
      theme,
      status,
      `zg collections index ${shellArg(info.name)}`,
    );
  }
}

export function printAnonymousInfo(
  info: ZvecGrepInfoResult,
  options: CliOptions,
): void {
  const theme = createStatusTheme(options);

  printField(theme, "root", theme.path(info.root));
  printField(theme, "policy", info.indexPolicy);
  printField(
    theme,
    "indexed",
    info.indexed ? theme.success("yes") : theme.warning("no"),
  );
  printField(theme, "state", formatAnonymousState(theme, info));
  printField(
    theme,
    "source",
    info.source === "index"
      ? theme.success(info.source)
      : theme.warning(info.source),
  );
  printField(theme, "home", theme.path(info.home));
  printField(theme, "index", theme.path(info.indexPath));

  if (info.collection?.embedding) {
    printField(
      theme,
      "embedding",
      `${info.collection.embedding.provider}/${info.collection.embedding.model}`,
    );
    printField(theme, "dimension", String(info.collection.embedding.dimension));
    printField(theme, "metric", info.collection.embedding.metric);
  } else if (info.collection) {
    printField(theme, "embedding", theme.warning("none"));
  }

  if (info.collection) {
    printField(
      theme,
      "roots",
      theme.path(info.collection.rootPaths.map(formatRootPath).join(", ")),
    );
  }

  if (info.status) {
    printIndexStatus(theme, info.status);
  }

  const suggestion =
    info.status && statusNeedsRefresh(info.status)
      ? "zg index"
      : info.suggestion;
  if (suggestion) {
    printField(theme, "suggestion", theme.accent(suggestion));
  }

  if (info.status) {
    printFailedFilesNote(theme, info.status, "zg index");
  }
}

export function printServerIndexInfo(
  info: {
    root: string;
    indexed: boolean;
    index_policy: "enabled" | "disabled" | "undecided";
    source: "index" | "unindexed";
    persistent: {
      home: string;
      index_path: string;
      collection?: {
        root_paths: Array<{
          absolute_path: string;
          recursive: boolean;
          include?: string[];
          exclude?: string[];
          globs?: string[];
          insensitive_globs?: string[];
          file_types?: string[];
          excluded_file_types?: string[];
          hidden?: boolean;
          no_ignore?: boolean;
          ignore_files?: string[];
          max_depth?: number;
          max_file_size_bytes?: number;
          follow?: boolean;
        }>;
        embedding?: {
          provider: string;
          model: string;
          dimension: number;
          metric: string;
        } | null;
      };
      files?: {
        stored: number;
        indexed: number;
        pending: number;
        failed: number;
        entities: number;
      };
      suggestion?: string;
    };
    runtime?: {
      job_state?: "queued" | "running" | "succeeded" | "failed" | "cancelled";
      progress?: {
        files_total?: number;
        files_indexed?: number;
        files_failed?: number;
      };
      error?: {
        code: string;
        message: string;
      };
    };
  },
  options: CliOptions,
): void {
  const theme = createStatusTheme(options);
  const jobState = info.runtime?.job_state;
  const stale = jobState === "queued" || jobState === "running";
  const failed = jobState === "failed" || jobState === "cancelled";
  const state = failed
    ? theme.danger("failed")
    : stale
      ? theme.warning("stale")
      : info.indexed
        ? theme.success("ready")
        : theme.warning("unindexed");

  printField(theme, "root", theme.path(info.root));
  printField(theme, "policy", info.index_policy);
  printField(
    theme,
    "indexed",
    info.indexed ? theme.success("yes") : theme.warning("no"),
  );
  printField(theme, "state", state);
  printField(theme, "source", info.source);
  printField(theme, "home", theme.path(info.persistent.home));
  printField(theme, "index", theme.path(info.persistent.index_path));

  const embedding = info.persistent.collection?.embedding;
  if (embedding) {
    printField(theme, "embedding", `${embedding.provider}/${embedding.model}`);
    printField(theme, "dimension", String(embedding.dimension));
    printField(theme, "metric", embedding.metric);
  }
  const rootPaths = info.persistent.collection?.root_paths;
  if (rootPaths?.length) {
    printField(
      theme,
      "roots",
      theme.path(
        rootPaths
          .map((root) =>
            formatRootPath({
              absolutePath: root.absolute_path,
              recursive: root.recursive,
              include: root.include,
              exclude: root.exclude,
              globs: root.globs,
              insensitiveGlobs: root.insensitive_globs,
              fileTypes: root.file_types,
              excludedFileTypes: root.excluded_file_types,
              hidden: root.hidden,
              noIgnore: root.no_ignore,
              ignoreFiles: root.ignore_files,
              maxDepth: root.max_depth,
              maxFileSizeBytes: root.max_file_size_bytes,
              follow: root.follow,
            }),
          )
          .join(", "),
      ),
    );
  }
  const files = info.persistent.files;
  if (files) {
    printField(theme, "files", `${files.indexed}/${files.stored} indexed`);
    printField(theme, "entities", String(files.entities));
    printField(theme, "pending", String(files.pending));
    printField(theme, "failed", String(files.failed));
  }
  if (jobState) printField(theme, "job", jobState);
  const progress = info.runtime?.progress;
  if (progress?.files_indexed !== undefined) {
    printField(
      theme,
      "progress",
      progress.files_total === undefined
        ? String(progress.files_indexed)
        : `${progress.files_indexed}/${progress.files_total}`,
    );
  }
  if (info.runtime?.error) {
    printField(theme, "error-code", info.runtime.error.code);
    printField(theme, "error", info.runtime.error.message);
  }
  if (info.persistent.suggestion) {
    printField(theme, "suggestion", theme.accent(info.persistent.suggestion));
  }
}

function formatAnonymousState(
  theme: StatusTheme,
  info: ZvecGrepInfoResult,
): string {
  const state = anonymousState(info);
  if (state === "ready") {
    return theme.success(state);
  }

  if (state === "failed") {
    return theme.danger(state);
  }

  return theme.warning(state);
}

function anonymousState(
  info: ZvecGrepInfoResult,
): "ready" | "stale" | "failed" | "disabled" | "unindexed" | "undecided" {
  if (info.indexPolicy === "disabled") {
    return "disabled";
  }

  if (!info.collection) {
    return "undecided";
  }

  if (!info.indexed) {
    return "unindexed";
  }

  if (info.status?.filesFailed && info.status.filesFailed > 0) {
    return "failed";
  }

  if (info.status && statusNeedsRefresh(info.status)) {
    return "stale";
  }

  return "ready";
}

export function printIndexResult(
  label: string,
  result: IndexResult,
  options: CliOptions,
  rootPaths?: CollectionInfo["rootPaths"],
): void {
  const theme = createStatusTheme(options);

  console.log(theme.accent(label));
  printField(
    theme,
    "files",
    `${result.filesScanned} scanned, ` +
      `${theme.success(String(result.filesAdded))} added, ` +
      `${theme.warning(String(result.filesModified))} modified, ` +
      `${theme.warning(String(result.filesPending))} retried, ` +
      `${theme.muted(String(result.filesUnchanged))} unchanged, ` +
      `${theme.muted(String(result.filesDeleted))} deleted, ` +
      failedCount(theme, result.filesFailed),
  );
  printField(theme, "entities", String(result.entitiesCreated));
  printField(
    theme,
    "duration",
    `${formatDuration(result.durationMs)} ${theme.muted(`(${result.durationMs}ms)`)}`,
  );
  if (rootPaths && rootPaths.length > 0) {
    printField(
      theme,
      "roots",
      theme.path(rootPaths.map(formatRootPath).join(", ")),
    );
  }
  printQueryFilters(theme, options);
}

export function printIndexPathFilterTip(options: CliOptions): void {
  if (
    options.globs !== undefined ||
    options.insensitiveGlobs !== undefined ||
    options.fileTypes !== undefined ||
    options.excludedFileTypes !== undefined
  ) {
    return;
  }

  const theme = createStatusTheme(options);
  printField(
    theme,
    "tip",
    theme.warning(
      "Default indexing skips common noise. For large or remote-embedding indexes, inspect this long-lived workspace and choose focused -g/--glob and -t/--type filters.",
    ),
  );
}

export function printCollectionRemoveResult(
  name: string,
  removed: boolean,
  options: CliOptions,
): void {
  const theme = createStatusTheme(options);
  console.log(
    removed
      ? `${theme.success("Removed")} collection ${theme.accent(name)}`
      : `${theme.warning("Collection not found")}: ${theme.accent(name)}`,
  );
}

function printIndexStatus(
  theme: StatusTheme,
  status: CollectionIndexStatus,
): void {
  printField(
    theme,
    "files",
    `${status.filesIndexed}/${status.filesScanned} indexed`,
  );
  printField(theme, "entities", String(status.entitiesIndexed ?? 0));
  printField(
    theme,
    "fresh",
    statusNeedsRefresh(status) ? theme.warning("no") : theme.success("yes"),
  );
  printField(theme, "changed", changedCount(theme, status));
  printField(
    theme,
    "pending",
    status.filesPending > 0
      ? theme.warning(String(status.filesPending))
      : theme.success(String(status.filesPending)),
  );
  printField(theme, "failed", failedCount(theme, status.filesFailed));
}

function statusNeedsRefresh(status: CollectionIndexStatus): boolean {
  return (
    status.filesAdded > 0 ||
    status.filesModified > 0 ||
    status.filesDeleted > 0 ||
    status.filesPending > 0 ||
    status.filesFailed > 0
  );
}

function printFailedFilesNote(
  theme: StatusTheme,
  status: CollectionIndexStatus,
  retryCommand: string,
): void {
  if (status.filesFailed === 0) {
    return;
  }

  printField(
    theme,
    "note",
    theme.warning(
      `retry ${retryCommand}; if failures persist, fix failed files or embedding configuration`,
    ),
  );

  const reasons = summarizeFailedFileReasons(status.failedFiles, retryCommand);
  if (reasons) {
    printField(theme, "failed_reasons", theme.danger(reasons));
  }
}

function summarizeFailedFileReasons(
  files: CollectionIndexStatus["failedFiles"],
  retryCommand: string,
): string | undefined {
  const withReasons = files.filter((file) => file.indexStatus?.error);
  if (withReasons.length === 0) {
    return undefined;
  }

  const shown = withReasons
    .slice(0, 3)
    .map(
      (file) =>
        `${file.relativePath}: ${clipReason(explainStoredFailureReason(file.indexStatus!.error!, retryCommand))}`,
    );
  const remaining = withReasons.length - shown.length;

  return remaining > 0
    ? `${shown.join("; ")}; and ${remaining} more`
    : shown.join("; ");
}

function explainStoredFailureReason(
  reason: string,
  retryCommand: string,
): string {
  const legacy =
    /ZVEC_GREP\.ENGINE\.INDEXING\.FILE_FAILED: Indexing file failed \(.*\bstage=([^) ]+)/.exec(
      reason,
    );
  if (!legacy) {
    return reason;
  }

  return `${legacy[1]} failed; this stored failure was recorded without the underlying cause, rerun ${retryCommand} to refresh the detailed error`;
}

function clipReason(reason: string): string {
  const compact = reason.replace(/\s+/g, " ").trim();
  const maxLength = 240;

  return compact.length > maxLength
    ? `${compact.slice(0, maxLength - 3)}...`
    : compact;
}

function changedCount(
  theme: StatusTheme,
  status: CollectionIndexStatus,
): string {
  const count = status.filesAdded + status.filesModified + status.filesDeleted;
  const value = `${status.filesAdded} added, ${status.filesModified} modified, ${status.filesDeleted} deleted`;

  return count > 0 ? theme.warning(value) : theme.success(value);
}

function printQueryFilters(theme: StatusTheme, options: CliOptions): void {
  if (options.modifiedAfter !== undefined) {
    printField(
      theme,
      "modified_after",
      formatTimeFilter(options.modifiedAfter),
    );
  }

  if (options.modifiedBefore !== undefined) {
    printField(
      theme,
      "modified_before",
      formatTimeFilter(options.modifiedBefore),
    );
  }
}

function printField(theme: StatusTheme, label: string, value: string): void {
  console.log(`${theme.label(label)}\t${value}`);
}

function failedCount(theme: StatusTheme, count: number): string {
  return count > 0
    ? theme.danger(`${count} failed`)
    : theme.success(`${count} failed`);
}

function createStatusTheme(options: CliOptions): StatusTheme {
  if (!shouldUseColor(options)) {
    return {
      label: identity,
      path: identity,
      success: identity,
      warning: identity,
      danger: identity,
      accent: identity,
      muted: identity,
    };
  }

  return {
    label: (value) => `\x1b[2m${value}\x1b[0m`,
    path: (value) => `\x1b[36m${value}\x1b[0m`,
    success: (value) => `\x1b[32m${value}\x1b[0m`,
    warning: (value) => `\x1b[33m${value}\x1b[0m`,
    danger: (value) => `\x1b[31m${value}\x1b[0m`,
    accent: (value) => `\x1b[1m${value}\x1b[0m`,
    muted: (value) => `\x1b[2m${value}\x1b[0m`,
  };
}

function formatRootPath(root: CollectionInfo["rootPaths"][number]): string {
  const filters = [
    root.include && root.include.length > 0
      ? `include=${root.include.join("|")}`
      : undefined,
    root.exclude && root.exclude.length > 0
      ? `exclude=${root.exclude.join("|")}`
      : undefined,
    root.globs && root.globs.length > 0
      ? `glob=${root.globs.join("|")}`
      : undefined,
    root.insensitiveGlobs && root.insensitiveGlobs.length > 0
      ? `iglob=${root.insensitiveGlobs.join("|")}`
      : undefined,
    root.fileTypes && root.fileTypes.length > 0
      ? `type=${root.fileTypes.join("|")}`
      : undefined,
    root.excludedFileTypes && root.excludedFileTypes.length > 0
      ? `type-not=${root.excludedFileTypes.join("|")}`
      : undefined,
    root.hidden ? "hidden" : undefined,
    root.noIgnore ? "no-ignore" : undefined,
    root.ignoreFiles && root.ignoreFiles.length > 0
      ? `ignore-file=${root.ignoreFiles.join("|")}`
      : undefined,
    root.maxDepth !== undefined ? `max-depth=${root.maxDepth}` : undefined,
    root.maxFileSizeBytes !== undefined
      ? `max-filesize=${root.maxFileSizeBytes}`
      : undefined,
    root.follow ? "follow" : undefined,
  ].filter((part): part is string => part !== undefined);

  return filters.length > 0
    ? `${root.absolutePath} (${filters.join(" ")})`
    : root.absolutePath;
}

function formatDuration(ms: number): string {
  if (ms < 1000) {
    return `${ms}ms`;
  }

  const totalSeconds = Math.round(ms / 1000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;

  if (minutes === 0) {
    return `${seconds}s`;
  }

  return `${minutes}m ${seconds}s`;
}

function formatTimeFilter(value: number): string {
  return `${new Date(value).toISOString()} (${value})`;
}

function identity(value: string): string {
  return value;
}

function shellArg(value: string): string {
  if (/^[A-Za-z0-9_./:=@+-]+$/.test(value)) {
    return value;
  }

  return `'${value.replaceAll("'", "'\\''")}'`;
}
