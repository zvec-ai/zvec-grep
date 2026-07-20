import type { IndexProgress } from "./engine/types.js";

const MCP_INDEX_PROGRESS_PREFIX = "zvec-grep:index-progress:";

export type IndexProgressUpdate = {
  phase: IndexProgress["phase"];
  line: string;
  progress: IndexProgress;
};

export function formatIndexProgressLine(
  progress: IndexProgress,
): string | undefined {
  if (progress.phase === "scanning") {
    return progress.detail;
  }

  if (progress.phase === "indexing") {
    const indexed = progress.filesIndexed ?? 0;
    const total = progress.filesTotal ?? 0;
    const detail = progress.detail ? ` ${truncate(progress.detail, 100)}` : "";
    const failed = formatFailedProgress(progress);
    const embedding = formatEmbeddingProgress(progress);
    return total > 0
      ? `Indexing files: ${indexed}/${total}${detail}${failed}${embedding}`
      : `Indexing files...${detail}${failed}${embedding}`;
  }

  return progress.detail ?? "Indexing complete";
}

export function indexProgressMessage(
  progress: IndexProgress,
): string | undefined {
  const line = formatIndexProgressLine(progress);
  return line
    ? `${MCP_INDEX_PROGRESS_PREFIX}${JSON.stringify({ line, progress })}`
    : undefined;
}

export function indexProgressFromMessage(
  message: string | undefined,
): IndexProgressUpdate | undefined {
  if (!message?.startsWith(MCP_INDEX_PROGRESS_PREFIX)) return undefined;
  const payload = message.slice(MCP_INDEX_PROGRESS_PREFIX.length);
  if (payload.startsWith("{")) {
    try {
      const decoded = JSON.parse(payload) as {
        line?: unknown;
        progress?: unknown;
      };
      if (
        typeof decoded.line === "string" &&
        isIndexProgress(decoded.progress)
      ) {
        return {
          phase: decoded.progress.phase,
          line: decoded.line,
          progress: decoded.progress,
        };
      }
    } catch {
      return undefined;
    }
  }

  // Backward-compatible decoding for progress emitted by an older daemon.
  const separator = payload.indexOf(":");
  if (separator < 0) return undefined;
  const phase = payload.slice(0, separator);
  if (phase !== "scanning" && phase !== "indexing" && phase !== "done") {
    return undefined;
  }
  const line = payload.slice(separator + 1);
  return { phase, line, progress: { phase, detail: line } };
}

function isIndexProgress(value: unknown): value is IndexProgress {
  if (!value || typeof value !== "object") return false;
  const progress = value as Record<string, unknown>;
  return (
    progress.phase === "scanning" ||
    progress.phase === "indexing" ||
    progress.phase === "done"
  );
}

function formatFailedProgress(progress: IndexProgress): string {
  const failed = progress.filesFailed ?? 0;
  return failed > 0 ? ` [failed=${failed}]` : "";
}

function formatEmbeddingProgress(progress: IndexProgress): string {
  const embedding = progress.embedding;
  if (!embedding) {
    return "";
  }

  const parts: string[] = [];
  if (typeof embedding.concurrency === "number") {
    parts.push(`concurrency=${embedding.concurrency}`);
  }

  if (
    typeof embedding.retryableFailures === "number" &&
    embedding.retryableFailures > 0
  ) {
    parts.push(`retries=${embedding.retryableFailures}`);
  }

  return parts.length > 0 ? ` [embed ${parts.join(" ")}]` : "";
}

function truncate(value: string, maxLength: number): string {
  if (value.length <= maxLength) {
    return value;
  }

  return `${value.slice(0, Math.max(0, maxLength - 1))}...`;
}
