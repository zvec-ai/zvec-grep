import { homedir } from "node:os";
import { isAbsolute, relative, sep } from "node:path";
import type {
  CollectionIndexStatus,
  CollectionInfo,
  IndexResult,
  ZvecGrepInfoResult,
} from "../../index.js";
import type { RemoteEmbeddingAuthorizationStatus } from "../../authorization/types.js";
import type { CliOptions } from "../types.js";
import { shouldUseColor } from "./highlight.js";
import { formatGreenProgressBar } from "./progress.js";
import {
  indexCompletionFromStatus,
  indexStatusNeedsRefresh as statusNeedsRefresh,
} from "../../engine/index-status.js";

type StatusTheme = {
  color: boolean;
  label(value: string): string;
  path(value: string): string;
  success(value: string): string;
  warning(value: string): string;
  danger(value: string): string;
  accent(value: string): string;
  muted(value: string): string;
};

export type WorkspaceIndexState =
  | "ready"
  | "indexing"
  | "stale"
  | "failed"
  | "cancelled"
  | "disabled"
  | "unindexed"
  | "undecided";

type WorkspaceRootPath = CollectionInfo["rootPaths"][number];

type WorkspaceStatusView = {
  root: string;
  indexPath: string;
  policy: "enabled" | "disabled" | "undecided";
  state: WorkspaceIndexState;
  embedding?: CollectionInfo["embedding"];
  roots?: readonly WorkspaceRootPath[];
  files?: {
    indexed: number;
    total: number;
    entities: number;
    truncated: number;
    pending: number;
    failed: number;
  };
  changes?: {
    added: number;
    modified: number;
    deleted: number;
  };
  suggestion?: string;
  error?: {
    code: string;
    message: string;
  };
  failedReasons?: string;
};

type ServerRootPath = {
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
};

type ServerIndexInfo = {
  root: string;
  indexed: boolean;
  index_policy: "enabled" | "disabled" | "undecided";
  source: "index" | "unindexed";
  persistent: {
    home: string;
    index_path: string;
    collection?: {
      root_paths: ServerRootPath[];
      embedding?: {
        provider: string;
        model: string;
        dimension: number;
        metric: string;
      } | null;
    };
    files?: {
      stored: number;
      scanned?: number;
      indexed: number;
      pending: number;
      failed: number;
      added: number;
      modified: number;
      deleted: number;
      unchanged: number;
      entities: number;
      truncated_fragments?: number;
    };
    suggestion?: string;
  };
  runtime?: {
    watcher_active?: boolean;
    dirty_revision?: number;
    indexed_revision?: number;
    active_job_id?: string;
    job_state?: "queued" | "running" | "succeeded" | "failed" | "cancelled";
    progress?: {
      files_total?: number;
      files_indexed?: number;
      files_failed?: number;
    };
    completion?: {
      completed: number;
      total: number;
    };
    error?: {
      code: string;
      message: string;
    };
  };
};

export function printRemoteEmbeddingAuthorizationStatus(
  root: string,
  status: RemoteEmbeddingAuthorizationStatus,
  options: CliOptions,
): void {
  const theme = createStatusTheme(options);
  const validGrants = status.grants.filter((grant) => grant.valid);

  if (status.grants.length === 0) {
    console.log(theme.warning("○ Remote Embedding is not authorized"));
  } else if (validGrants.length === status.grants.length) {
    console.log(theme.success("✓ Remote Embedding is authorized"));
  } else if (validGrants.length === 0) {
    console.log(theme.danger("✗ Remote Embedding authorization is invalid"));
  } else {
    console.log(
      theme.warning("! Remote Embedding authorization needs attention"),
    );
  }
  console.log(`  ${theme.path(formatDisplayPath(root))}`);

  if (status.grants.length === 0) {
    console.log("");
    console.log(theme.accent("Run"));
    console.log(
      `  ${theme.path("zg auth grant --capability embedding --scope workspace")}`,
    );
    return;
  }

  console.log("");
  console.log(theme.accent("Authorization"));
  status.grants.forEach((grant, index) => {
    if (index > 0) console.log("");
    for (const line of formatStatusField(
      theme,
      "Scope",
      grant.valid
        ? theme.success("Workspace")
        : theme.danger("Invalid Workspace grant"),
    )) {
      console.log(line);
    }
    for (const line of formatStatusField(
      theme,
      "Target",
      `${grant.provider}/${grant.model}`,
    )) {
      console.log(line);
    }
    for (const line of formatStatusField(
      theme,
      "Endpoint",
      formatEndpointHost(grant.endpoint),
    )) {
      console.log(line);
    }
  });

  console.log("");
  console.log(theme.accent("Storage"));
  for (const line of formatStatusField(
    theme,
    "Grant",
    theme.path(formatStoragePath(status.path, root)),
  )) {
    console.log(line);
  }
}

export function printWorkspaceInfo(
  info: ZvecGrepInfoResult,
  options: CliOptions,
): WorkspaceIndexState {
  const theme = createStatusTheme(options);
  const status = info.status ?? undefined;
  const completion = indexCompletionFromStatus(status);
  const suggestion =
    status && statusNeedsRefresh(status) ? "zg index" : info.suggestion;

  const state = workspaceState(info);
  printWorkspaceIndexStatus(theme, {
    root: info.root,
    indexPath: info.indexPath,
    policy: info.indexPolicy,
    state,
    embedding: info.collection?.embedding,
    roots: info.collection?.rootPaths,
    files: status
      ? {
          indexed: completion?.completed ?? 0,
          total: completion?.total ?? 0,
          entities: status.entitiesIndexed ?? 0,
          truncated: status.fragmentsTruncated ?? 0,
          pending: status.filesPending,
          failed: status.filesFailed,
        }
      : undefined,
    changes: status
      ? {
          added: status.filesAdded,
          modified: status.filesModified,
          deleted: status.filesDeleted,
        }
      : undefined,
    suggestion,
    failedReasons: status
      ? summarizeFailedFileReasons(status.failedFiles, "zg index")
      : undefined,
  });
  return state;
}

export function printServerIndexInfo(
  info: ServerIndexInfo,
  options: CliOptions,
): WorkspaceIndexState {
  const theme = createStatusTheme(options);
  const state = serverIndexState(info);
  const embedding = info.persistent.collection?.embedding;
  const roots = info.persistent.collection?.root_paths.map(mapServerRootPath);
  const files = info.persistent.files;
  const completion = info.runtime?.completion;

  printWorkspaceIndexStatus(theme, {
    root: info.root,
    indexPath: info.persistent.index_path,
    policy: info.index_policy,
    state,
    embedding,
    roots,
    files: files
      ? {
          indexed: completion?.completed ?? files.unchanged,
          total:
            completion?.total ??
            files.scanned ??
            Math.max(0, files.stored + files.added - files.deleted),
          entities: files.entities,
          truncated: files.truncated_fragments ?? 0,
          pending: files.pending,
          failed: info.runtime?.progress?.files_failed ?? files.failed,
        }
      : undefined,
    changes: files
      ? {
          added: files.added,
          modified: files.modified,
          deleted: files.deleted,
        }
      : undefined,
    suggestion: info.persistent.suggestion,
    error: info.runtime?.error,
  });
  return state;
}

function printWorkspaceIndexStatus(
  theme: StatusTheme,
  view: WorkspaceStatusView,
): void {
  console.log(formatWorkspaceStatusHeading(theme, view.state));
  console.log(`  ${theme.path(formatDisplayPath(view.root))}`);

  const sections: string[][] = [];
  if (view.files) {
    const { indexed, total, entities, truncated, pending, failed } = view.files;
    const percent =
      total === 0
        ? 0
        : Math.min(100, Math.round((Math.max(indexed, 0) / total) * 100));
    const bar = formatGreenProgressBar(indexed, total, 20, theme.color);
    const summary = [
      ...formatStatusField(
        theme,
        "Coverage",
        `${bar} ${String(percent).padStart(3)}%  ${formatCount(indexed)} / ${formatCount(total)} files`,
      ),
      ...formatStatusField(theme, "Entities", formatCount(entities)),
      ...formatStatusField(
        theme,
        "Truncated",
        truncated > 0
          ? theme.warning(`${formatCount(truncated)} fragments`)
          : theme.muted("0 fragments"),
      ),
      ...formatStatusField(
        theme,
        "Queue",
        `${statusCount(theme, pending, "pending", "warning")} ${theme.muted("·")} ${statusCount(theme, failed, "failed", "danger")}`,
      ),
    ];

    if (view.changes && changedTotal(view.changes) > 0) {
      summary.push(
        ...formatStatusField(
          theme,
          "Changes",
          `${theme.warning(`${formatCount(view.changes.added)} added`)} ${theme.muted("·")} ${theme.warning(`${formatCount(view.changes.modified)} modified`)} ${theme.muted("·")} ${theme.warning(`${formatCount(view.changes.deleted)} deleted`)}`,
        ),
      );
    }
    sections.push(summary);
  }

  if (view.embedding !== undefined) {
    sections.push(
      formatStatusField(
        theme,
        "Embedding",
        view.embedding
          ? [
              `${view.embedding.provider}/${view.embedding.model}`,
              `${formatCount(view.embedding.dimension)} dimensions ${theme.muted("·")} ${view.embedding.metric}`,
            ]
          : theme.warning("Not configured"),
      ),
    );
  }

  const storage = formatStatusField(
    theme,
    "Storage",
    theme.path(formatStoragePath(view.indexPath, view.root)),
  );
  if (view.roots && shouldShowRoots(view.root, view.roots)) {
    storage.push(
      ...formatStatusField(
        theme,
        "Roots",
        view.roots.map((root) => theme.path(formatDisplayRootPath(root))),
      ),
    );
  }
  if (view.policy !== "enabled") {
    storage.push(
      ...formatStatusField(
        theme,
        "Policy",
        view.policy === "disabled"
          ? theme.warning(view.policy)
          : theme.muted(view.policy),
      ),
    );
  }
  sections.push(storage);

  const diagnostics: string[] = [];
  if (view.error) {
    diagnostics.push(
      ...formatStatusField(theme, "Error", [
        theme.danger(view.error.code),
        theme.danger(view.error.message),
      ]),
    );
  }
  if (view.failedReasons) {
    diagnostics.push(
      ...formatStatusField(theme, "Problem", theme.danger(view.failedReasons)),
    );
  }
  if (view.suggestion) {
    diagnostics.push(
      ...formatStatusField(theme, "Next", theme.accent(view.suggestion)),
    );
  }
  if (diagnostics.length > 0) {
    sections.push(diagnostics);
  }

  for (const section of sections) {
    if (section.length === 0) continue;
    console.log("");
    for (const line of section) console.log(line);
  }
}

function formatWorkspaceStatusHeading(
  theme: StatusTheme,
  state: WorkspaceIndexState,
): string {
  switch (state) {
    case "ready":
      return theme.success("✓ Workspace index is ready");
    case "indexing":
      return theme.warning("◐ Workspace index is updating");
    case "stale":
      return theme.warning("! Workspace index needs an update");
    case "failed":
      return theme.danger("✗ Workspace index failed");
    case "cancelled":
      return theme.warning("! Workspace index update was cancelled");
    case "disabled":
      return theme.warning("○ Workspace indexing is disabled");
    case "unindexed":
      return theme.warning("○ Workspace index is not created");
    case "undecided":
      return theme.warning("? Workspace index is not configured");
  }
}

function formatStatusField(
  theme: StatusTheme,
  label: string,
  value: string | readonly string[],
): string[] {
  const values = typeof value === "string" ? [value] : value;
  const labelWidth = 12;
  const continuation = `  ${" ".repeat(labelWidth)}`;
  return values.map((line, index) =>
    index === 0
      ? `  ${theme.label(label.padEnd(labelWidth))}${line}`
      : `${continuation}${line}`,
  );
}

function statusCount(
  theme: StatusTheme,
  count: number,
  label: string,
  tone: "warning" | "danger",
): string {
  const value = `${formatCount(count)} ${label}`;
  return count > 0 ? theme[tone](value) : theme.muted(value);
}

function changedTotal(
  changes: NonNullable<WorkspaceStatusView["changes"]>,
): number {
  return changes.added + changes.modified + changes.deleted;
}

function serverIndexState(info: ServerIndexInfo): WorkspaceIndexState {
  if (info.index_policy === "disabled") return "disabled";
  if (info.runtime?.job_state === "failed" || info.runtime?.error) {
    return "failed";
  }
  if (info.runtime?.job_state === "cancelled") return "cancelled";
  if (
    info.runtime?.job_state === "queued" ||
    info.runtime?.job_state === "running"
  ) {
    return "indexing";
  }
  if (
    info.runtime?.dirty_revision !== undefined &&
    info.runtime.indexed_revision !== undefined &&
    info.runtime.dirty_revision > info.runtime.indexed_revision
  ) {
    return "stale";
  }
  if ((info.persistent.files?.failed ?? 0) > 0) return "failed";
  if (
    (info.persistent.files?.pending ?? 0) > 0 ||
    serverChangedTotal(info.persistent.files) > 0
  ) {
    return "stale";
  }
  if (info.indexed) return "ready";
  return info.index_policy === "undecided" ? "undecided" : "unindexed";
}

function serverChangedTotal(
  files: ServerIndexInfo["persistent"]["files"],
): number {
  return (files?.added ?? 0) + (files?.modified ?? 0) + (files?.deleted ?? 0);
}

function mapServerRootPath(root: ServerRootPath): WorkspaceRootPath {
  return {
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
  };
}

function shouldShowRoots(
  workspaceRoot: string,
  roots: readonly WorkspaceRootPath[],
): boolean {
  return (
    roots.length > 1 ||
    roots.some(
      (root) =>
        root.absolutePath !== workspaceRoot ||
        formatRootPath(root) !== root.absolutePath,
    )
  );
}

function formatDisplayRootPath(root: WorkspaceRootPath): string {
  const formatted = formatRootPath(root);
  return `${formatDisplayPath(root.absolutePath)}${formatted.slice(root.absolutePath.length)}`;
}

function formatStoragePath(indexPath: string, workspaceRoot: string): string {
  const localPath = relative(workspaceRoot, indexPath);
  if (
    localPath.length > 0 &&
    !isAbsolute(localPath) &&
    localPath !== ".." &&
    !localPath.startsWith(`..${sep}`)
  ) {
    return localPath;
  }
  return formatDisplayPath(indexPath);
}

function formatDisplayPath(path: string): string {
  const home = homedir();
  if (path === home) return "~";
  return path.startsWith(`${home}${sep}`)
    ? `~${path.slice(home.length)}`
    : path;
}

function formatEndpointHost(endpoint: string): string {
  try {
    return new URL(endpoint).host || endpoint;
  } catch {
    return endpoint;
  }
}

function formatCount(value: number): string {
  return value.toLocaleString("en-US");
}

function workspaceState(
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
  const color = shouldUseColor(options);
  if (!color) {
    return {
      color,
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
    color,
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
