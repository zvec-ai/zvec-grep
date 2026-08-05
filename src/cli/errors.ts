import { isEngineError } from "../engine/errors/index.js";
import type { ColorMode } from "./types.js";

export type ErrorPrintOptions = {
  color?: ColorMode;
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

  if (isEngineError(error)) {
    console.error(`${theme.error("Error:")} ${error.message}`);
    console.error(`${theme.label("Code:")} ${error.code}`);
    if (error.context) {
      console.error(`${theme.label("Details:")}`);
      for (const line of formatContextLines(error.context)) {
        console.error(`  ${line}`);
      }
    }
    printCause(error, theme);
    return;
  }

  console.error(
    `${theme.error("Error:")} ${error instanceof Error ? error.message : String(error)}`,
  );
  printCause(error, theme);
}

function formatContextLines(context: string): string[] {
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
    console.error(`${theme.label("Cause:")} ${cause}`);
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
