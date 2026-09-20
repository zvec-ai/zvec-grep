import type { WorkspaceIndexStatus, IndexProgress } from "./types.js";

export type IndexCompletion = {
  completed: number;
  total: number;
};

export function indexStatusNeedsRefresh(
  status: WorkspaceIndexStatus | null | undefined,
): boolean {
  return Boolean(
    status &&
    (status.filesAdded > 0 ||
      status.filesModified > 0 ||
      status.filesDeleted > 0 ||
      status.filesPending > 0 ||
      status.filesFailed > 0),
  );
}

export function indexCompletionFromStatus(
  status: WorkspaceIndexStatus | null | undefined,
): IndexCompletion | undefined {
  if (!status) return undefined;
  return {
    completed: status.filesUnchanged,
    total: status.filesScanned,
  };
}

export function mergeIndexCompletion(
  completion: IndexCompletion | undefined,
  progress: IndexProgress | undefined,
): IndexCompletion | undefined {
  const processed = progress?.filesIndexed;
  const total = progress?.filesTotal;
  const succeeded =
    processed === undefined
      ? undefined
      : Math.max(0, processed - (progress?.filesFailed ?? 0));
  if (!completion) {
    return succeeded === undefined || total === undefined
      ? undefined
      : { completed: succeeded, total };
  }
  return {
    completed: Math.min(
      completion.total,
      completion.completed + (succeeded ?? 0),
    ),
    total: completion.total,
  };
}

export function indexCompletionForJob(
  completion: IndexCompletion | undefined,
  state:
    "queued" | "running" | "succeeded" | "failed" | "cancelled" | undefined,
  progress: IndexProgress | undefined,
): IndexCompletion | undefined {
  return state === "running"
    ? mergeIndexCompletion(completion, progress)
    : completion;
}
