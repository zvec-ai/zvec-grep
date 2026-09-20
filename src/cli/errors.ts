import { isEngineError, redactErrorText } from "../engine/errors.js";
import type { ColorMode } from "./types.js";

export type ErrorPrintOptions = {
  color?: ColorMode;
  debug?: boolean;
};

type ErrorTheme = {
  error(value: string): string;
  label(value: string): string;
};

export function printError(
  error: unknown,
  options: ErrorPrintOptions = {},
): void {
  const theme = createErrorTheme(options.color);
  const downloadFailure = modelDownloadFailureMessage(error);
  if (downloadFailure && !options.debug) {
    console.error(`${theme.error("Error:")} ${downloadFailure}`);
    return;
  }

  if (isEngineError(error)) {
    console.error(
      `${theme.error("Error:")} ${redactErrorText(downloadFailure ?? error.message, 512)}`,
    );
    console.error(`${theme.label("Code:")} ${error.code}`);
    if (error.context) {
      console.error(`${theme.label("Details:")}`);
      for (const line of formatContextLines(
        redactErrorText(error.context, 4_096),
      )) {
        console.error(`  ${line}`);
      }
    }
    printCause(error, theme);
    return;
  }

  console.error(
    `${theme.error("Error:")} ${redactErrorText(error instanceof Error ? error.message : String(error), 512)}`,
  );
  printCause(error, theme);
}

export function modelDownloadFailureMessage(
  error: unknown,
): string | undefined {
  if (!error || typeof error !== "object" || !("code" in error)) {
    return undefined;
  }
  const context =
    "context" in error && typeof error.context === "string"
      ? error.context
      : "";
  const downloadCode = "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_DOWNLOAD_FAILED";
  let details = context;
  if (error.code !== downloadCode) {
    if (error.code !== "ZVEC_GREP.ENGINE.INDEXING.FILES_FAILED") {
      return undefined;
    }
    const failedReasons = context
      .split(/\r?\n/)
      .find((line) => line.startsWith("failedReasons="));
    if (!failedReasons) return undefined;
    const causeIndex = failedReasons.indexOf(`${downloadCode}:`);
    if (causeIndex < 0) return undefined;
    details = failedReasons.slice(causeIndex + downloadCode.length);
  }
  const model = details.match(
    /(?:^|[\s(])model=(local\/[A-Za-z0-9._/-]+)/,
  )?.[1];
  return model
    ? `Failed to download model "${model}".`
    : "Failed to download model.";
}

export function formatContextLines(context: string): string[] {
  return context
    .split(/\r?\n/)
    .flatMap(formatContextLine)
    .filter((line) => line.length > 0);
}

function formatContextLine(line: string): string[] {
  const trimmed = line.trim();
  if (trimmed.length === 0) {
    return [];
  }

  const pairs = parseKeyValuePairs(trimmed);

  return pairs.length > 0
    ? pairs.map((pair) => `${pair.key}: ${pair.value}`)
    : [trimmed];
}

function parseKeyValuePairs(line: string): { key: string; value: string }[] {
  const matches = [...line.matchAll(/(^|\s)([A-Za-z][A-Za-z0-9_.-]*)=/g)].map(
    (match) => ({
      key: match[2]!,
      keyStart: match.index! + match[1]!.length,
      valueStart: match.index! + match[0]!.length,
    }),
  );

  if (matches.length === 0) {
    return [];
  }

  return matches.map((match, index) => {
    const next = matches[index + 1];
    const valueEnd = next ? next.keyStart : line.length;

    return {
      key: match.key,
      value: line.slice(match.valueStart, valueEnd).trim(),
    };
  });
}

function printCause(error: unknown, theme: ErrorTheme): void {
  const cause = errorCauseMessage(error);
  if (cause) {
    console.error(`${theme.label("Cause:")} ${redactErrorText(cause, 512)}`);
  }
}

function errorCauseMessage(error: unknown): string | undefined {
  if (!(error instanceof Error) || error.cause === undefined) {
    return undefined;
  }

  return error.cause instanceof Error
    ? error.cause.message
    : String(error.cause);
}

export function colorModeFromArgs(args: readonly string[]): ColorMode {
  for (let index = 0; index < args.length; index++) {
    const arg = args[index];
    if (arg === "--no-color") {
      return "never";
    }

    if (arg === "--color") {
      const value = args[index + 1];
      if (value === "always" || value === "never" || value === "auto") {
        return value;
      }
    }
  }

  return "auto";
}

function createErrorTheme(mode: ColorMode = "auto"): ErrorTheme {
  const enabled = shouldUseErrorColor(mode);
  return {
    error: (value) => (enabled ? `\x1b[1;31m${value}\x1b[0m` : value),
    label: (value) => (enabled ? `\x1b[2m${value}\x1b[0m` : value),
  };
}

function shouldUseErrorColor(mode: ColorMode): boolean {
  if (mode === "always") {
    return true;
  }

  if (mode === "never") {
    return false;
  }

  return process.env.NO_COLOR === undefined && process.stderr.isTTY === true;
}
