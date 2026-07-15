import type { IndexProgress } from "../../index.js";

export type IndexProgressReporter = {
  report(progress: IndexProgress): void;
  finish(): void;
};

export type IndexProgressReporterOptions = {
  nonTtyIntervalMs?: number;
};

const NON_TTY_PROGRESS_INTERVAL_MS = 15_000;

export function createIndexProgressReporter(
  options: IndexProgressReporterOptions = {},
): IndexProgressReporter {
  const nonTtyIntervalMs =
    options.nonTtyIntervalMs ?? NON_TTY_PROGRESS_INTERVAL_MS;
  let lastLineLength = 0;
  let finished = false;
  let currentNonTtyLine = "";
  let lastNonTtyWriteTime = 0;
  let heartbeat: NodeJS.Timeout | null = null;

  const writeLine = (line: string, force = false): void => {
    if (process.stderr.isTTY) {
      lastLineLength = writeTtyProgressLine(line, lastLineLength);
      return;
    }

    currentNonTtyLine = line;
    startHeartbeat();

    const now = Date.now();
    if (
      force ||
      lastNonTtyWriteTime === 0 ||
      now - lastNonTtyWriteTime >= nonTtyIntervalMs
    ) {
      writeNonTtyProgressLine(line);
      lastNonTtyWriteTime = now;
    }
  };

  const startHeartbeat = (): void => {
    if (heartbeat || nonTtyIntervalMs <= 0) {
      return;
    }

    heartbeat = setInterval(() => {
      if (finished || currentNonTtyLine.length === 0) {
        return;
      }

      const now = Date.now();
      if (now - lastNonTtyWriteTime >= nonTtyIntervalMs) {
        writeNonTtyProgressLine(currentNonTtyLine);
        lastNonTtyWriteTime = now;
      }
    }, nonTtyIntervalMs);
    heartbeat.unref?.();
  };

  return {
    report(progress) {
      if (progress.phase === "scanning") {
        if (progress.detail) {
          writeLine(progress.detail);
        }
        return;
      }

      if (progress.phase === "indexing") {
        const indexed = progress.filesIndexed ?? 0;
        const total = progress.filesTotal ?? 0;
        const detail = progress.detail
          ? ` ${truncate(progress.detail, 100)}`
          : "";
        const failed = formatFailedProgress(progress);
        const embedding = formatEmbeddingProgress(progress);
        const line =
          total > 0
            ? `Indexing files: ${indexed}/${total}${detail}${failed}${embedding}`
            : `Indexing files...${detail}${failed}${embedding}`;

        writeLine(line);
        return;
      }

      if (progress.phase === "done") {
        const line = progress.detail ?? "Indexing complete";
        writeLine(line, true);
      }
    },
    finish() {
      if (finished) {
        return;
      }
      finished = true;

      if (heartbeat) {
        clearInterval(heartbeat);
        heartbeat = null;
      }

      if (process.stderr.isTTY && lastLineLength > 0) {
        process.stderr.write("\n");
      }
    },
  };
}

function writeTtyProgressLine(line: string, previousLength: number): number {
  const padding =
    previousLength > line.length
      ? " ".repeat(previousLength - line.length)
      : "";
  process.stderr.write(`\r${line}${padding}`);
  return line.length;
}

function writeNonTtyProgressLine(line: string): void {
  process.stderr.write(`${line}\n`);
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
