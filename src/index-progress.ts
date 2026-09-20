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
    const modelDownload = formatModelDownloadProgress(progress);
    if (modelDownload) {
      return modelDownload;
    }
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

export function formatModelDownloadProgress(
  progress: IndexProgress,
): string | undefined {
  const embedding = progress.embedding;
  if (!embedding?.model) {
    return undefined;
  }
  if (embedding.stage === "preparing") {
    return `Preparing ${embedding.model}`;
  }
  if (embedding.stage === "warning") {
    return `zvec-grep warning: ${singleLineText(
      embedding.message ?? "Embedding model warning",
    )}`;
  }
  if (embedding.stage === "ready") {
    return `Model ready: ${embedding.model}`;
  }
  if (embedding.stage !== "downloading") {
    return undefined;
  }

  const parts = [`Downloading ${embedding.model}`];
  const downloadedBytes = embedding.downloadedBytes;
  const totalBytes = embedding.totalBytes;
  if (
    typeof downloadedBytes === "number" &&
    typeof totalBytes === "number" &&
    totalBytes > 0
  ) {
    const percent = Math.min(
      100,
      Math.max(0, Math.round((downloadedBytes / totalBytes) * 100)),
    );
    parts.push(`${percent}%`);
    parts.push(
      `${formatByteCount(downloadedBytes)}/${formatByteCount(totalBytes)}`,
    );
  } else if (typeof downloadedBytes === "number") {
    parts.push(formatByteCount(downloadedBytes));
  }

  return parts.join(" · ");
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

function formatByteCount(value: number): string {
  const bytes = Math.max(0, value);
  const units = ["B", "KiB", "MiB", "GiB"];
  let amount = bytes;
  let unitIndex = 0;
  while (amount >= 1024 && unitIndex < units.length - 1) {
    amount /= 1024;
    unitIndex++;
  }
  const digits = unitIndex === 0 || amount >= 10 ? 0 : 1;
  return `${amount.toFixed(digits)} ${units[unitIndex]}`;
}

function singleLineText(value: string): string {
  let result = "";
  for (let index = 0; index < value.length; index++) {
    const code = value.charCodeAt(index);
    if (code === 0x1b && value.charCodeAt(index + 1) === 0x5b) {
      index += 2;
      while (index < value.length) {
        const ansiCode = value.charCodeAt(index);
        if (ansiCode >= 0x40 && ansiCode <= 0x7e) {
          break;
        }
        index++;
      }
      continue;
    }
    if (code < 0x20 || (code >= 0x7f && code <= 0x9f)) {
      result += " ";
      continue;
    }
    result += value[index];
  }
  return result.replace(/\s+/g, " ").trim();
}
