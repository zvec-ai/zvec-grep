import type { IndexProgress } from "../../index.js";
import { formatIndexProgressLine } from "../../index-progress.js";
import type { ColorMode } from "../types.js";

export type IndexProgressReporter = {
  report(progress: IndexProgress): void;
  reportLine(line: string, force?: boolean): void;
  finish(): void;
};

export type IndexProgressReporterOptions = {
  nonTtyIntervalMs?: number;
  color?: ColorMode;
};

const NON_TTY_PROGRESS_INTERVAL_MS = 15_000;
const DEFAULT_TTY_COLUMNS = 100;
const MAX_PROGRESS_BAR_WIDTH = 25;
const MIN_PROGRESS_BAR_WIDTH = 8;
const ANSI_RESET = "\x1b[0m";
const ANSI_DIM = "\x1b[2m";
const ANSI_HIDE_CURSOR = "\x1b[?25l";
const ANSI_SHOW_CURSOR = "\x1b[?25h";
const ANSI_CLEAR_LINE = "\r\x1b[2K";

const UNICODE_GLYPHS = {
  rail: "│",
  spinner: ["·", "✢", "✳", "✶", "✻", "✽"],
  done: "◆",
  barFilled: "█",
  barEmpty: "░",
};

const ASCII_GLYPHS = {
  rail: "|",
  spinner: [".", "*", "+", "x", "o", "O"],
  done: "*",
  barFilled: "#",
  barEmpty: "-",
};

export function createIndexProgressReporter(
  options: IndexProgressReporterOptions = {},
): IndexProgressReporter {
  const nonTtyIntervalMs =
    options.nonTtyIntervalMs ?? NON_TTY_PROGRESS_INTERVAL_MS;
  const color = shouldUseProgressColor(options.color ?? "auto");
  let finished = false;
  let ttyLineWritten = false;
  let ttyCursorHidden = false;
  let lastIndexingProgress: IndexProgress | undefined;
  let currentNonTtyLine = "";
  let lastNonTtyWriteTime = 0;
  let heartbeat: NodeJS.Timeout | null = null;

  const writeLine = (line: string, force = false): void => {
    if (process.stderr.isTTY) {
      if (!ttyCursorHidden) {
        process.stderr.write(ANSI_HIDE_CURSOR);
        ttyCursorHidden = true;
      }
      writeTtyProgressLine(line);
      ttyLineWritten = true;
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
      if (process.stderr.isTTY) {
        if (progress.phase === "indexing") {
          lastIndexingProgress = progress;
        }
        const line = formatTtyProgressLine(
          progress,
          lastIndexingProgress,
          color,
        );
        if (line) writeLine(line);
        return;
      }

      const line = formatIndexProgressLine(progress);
      if (line) writeLine(line, progress.phase === "done");
    },
    reportLine(line, force = false) {
      writeLine(line, force);
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

      if (process.stderr.isTTY) {
        if (ttyLineWritten) {
          process.stderr.write("\n");
        }
        if (ttyCursorHidden) {
          process.stderr.write(ANSI_SHOW_CURSOR);
          ttyCursorHidden = false;
        }
      }
    },
  };
}

function writeTtyProgressLine(line: string): void {
  process.stderr.write(`${ANSI_CLEAR_LINE}${line}`);
}

function writeNonTtyProgressLine(line: string): void {
  process.stderr.write(`${line}\n`);
}

function formatTtyProgressLine(
  progress: IndexProgress,
  lastIndexingProgress: IndexProgress | undefined,
  color: boolean,
): string | undefined {
  const glyphs = supportsUnicode() ? UNICODE_GLYPHS : ASCII_GLYPHS;
  if (progress.phase === "scanning") {
    const count = progress.filesTotal
      ? `  ${formatCount(progress.filesTotal)} files`
      : "";
    return `${dim(glyphs.rail, color)}  ${green(glyphs.spinner[0]!, color)} Scanning workspace${count}`;
  }

  const effectiveProgress =
    progress.phase === "done" && lastIndexingProgress
      ? {
          ...lastIndexingProgress,
          filesIndexed:
            lastIndexingProgress.filesTotal ??
            lastIndexingProgress.filesIndexed,
        }
      : progress;
  const total = effectiveProgress.filesTotal ?? 0;
  if (total <= 0) {
    const label =
      progress.phase === "done"
        ? (progress.detail ?? "Indexing complete")
        : "Indexing files";
    const glyph =
      progress.phase === "done"
        ? glyphs.done
        : glyphs.spinner[
            (effectiveProgress.filesIndexed ?? 0) % glyphs.spinner.length
          ]!;
    return `${dim(glyphs.rail, color)}  ${green(glyph, color)} ${label}`;
  }

  const indexed = Math.min(
    Math.max(effectiveProgress.filesIndexed ?? 0, 0),
    total,
  );
  const percent = Math.min(100, Math.round((indexed / total) * 100));
  const glyph =
    progress.phase === "done"
      ? glyphs.done
      : glyphs.spinner[indexed % glyphs.spinner.length]!;
  const plainPrefix = `${glyphs.rail}  ${glyph} Indexing files  `;
  const prefix = `${dim(glyphs.rail, color)}  ${green(glyph, color)} Indexing files  `;
  const count = `${formatCount(indexed)}/${formatCount(total)}`;
  const coreSuffix = `  ${String(percent).padStart(3)}%  ${count}`;
  const metadata = formatTtyMetadata(effectiveProgress);
  let suffix = `${coreSuffix}${metadata}`;
  const columns = process.stderr.columns ?? DEFAULT_TTY_COLUMNS;
  let available = columns - plainPrefix.length - suffix.length - 1;
  if (available < MIN_PROGRESS_BAR_WIDTH && metadata) {
    suffix = coreSuffix;
    available = columns - plainPrefix.length - suffix.length - 1;
  }
  const barWidth = Math.max(
    MIN_PROGRESS_BAR_WIDTH,
    Math.min(MAX_PROGRESS_BAR_WIDTH, available),
  );
  const filled = Math.round((barWidth * percent) / 100);
  return `${prefix}${gradientBar(filled, barWidth, glyphs, color)}${suffix}`;
}

function gradientBar(
  filled: number,
  width: number,
  glyphs: typeof UNICODE_GLYPHS | typeof ASCII_GLYPHS,
  color: boolean,
): string {
  const boundedFilled = Math.min(Math.max(filled, 0), width);
  if (!color) {
    return `${glyphs.barFilled.repeat(boundedFilled)}${glyphs.barEmpty.repeat(width - boundedFilled)}`;
  }

  let bar = "";
  for (let index = 0; index < boundedFilled; index++) {
    const ratio = boundedFilled <= 1 ? 1 : index / (boundedFilled - 1);
    const red = interpolate(22, 134, ratio);
    const green = interpolate(163, 239, ratio);
    const blue = interpolate(74, 172, ratio);
    bar += `\x1b[38;2;${red};${green};${blue}m${glyphs.barFilled}`;
  }
  return `${bar}${ANSI_RESET}${ANSI_DIM}${glyphs.barEmpty.repeat(width - boundedFilled)}${ANSI_RESET}`;
}

export function formatGreenProgressBar(
  completed: number,
  total: number,
  width: number,
  color: boolean,
): string {
  const boundedTotal = Math.max(total, 0);
  const boundedWidth = Math.max(Math.floor(width), 0);
  const ratio =
    boundedTotal === 0
      ? 0
      : Math.min(Math.max(completed, 0), boundedTotal) / boundedTotal;
  const filled = Math.round(boundedWidth * ratio);
  const glyphs = supportsUnicode() ? UNICODE_GLYPHS : ASCII_GLYPHS;
  return gradientBar(filled, boundedWidth, glyphs, color);
}

function formatTtyMetadata(progress: IndexProgress): string {
  const parts: string[] = [];
  const failed = progress.filesFailed ?? 0;
  if (failed > 0) parts.push(`${formatCount(failed)} failed`);
  const retries = progress.embedding?.retryableFailures ?? 0;
  if (retries > 0) parts.push(`${formatCount(retries)} retries`);
  const concurrency = progress.embedding?.concurrency;
  if (concurrency && concurrency > 0) {
    parts.push(`${formatCount(concurrency)} workers`);
  }
  return parts.length > 0 ? `  ${parts.join(" · ")}` : "";
}

function shouldUseProgressColor(mode: ColorMode): boolean {
  if (mode === "always") return true;
  if (mode === "never") return false;
  return process.env.NO_COLOR === undefined && process.stderr.isTTY === true;
}

function supportsUnicode(): boolean {
  return process.platform !== "win32" && process.env.TERM !== "linux";
}

function green(value: string, enabled: boolean): string {
  return enabled ? `\x1b[38;2;74;222;128m${value}${ANSI_RESET}` : value;
}

function dim(value: string, enabled: boolean): string {
  return enabled ? `${ANSI_DIM}${value}${ANSI_RESET}` : value;
}

function interpolate(start: number, end: number, ratio: number): number {
  return Math.round(start + (end - start) * ratio);
}

function formatCount(value: number): string {
  return value.toLocaleString("en-US");
}
