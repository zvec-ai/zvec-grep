export const ENGINE_ERROR_CODE_PREFIX = "ZVEC_GREP.ENGINE";

export type EngineErrorCode = `${typeof ENGINE_ERROR_CODE_PREFIX}.${string}`;

export type EngineErrorOptions = {
  code: EngineErrorCode;
  context?: string;
  cause?: unknown;
};

export class EngineError extends Error {
  readonly code: EngineErrorCode;
  readonly context?: string;

  constructor(message: string, options: EngineErrorOptions) {
    super(message, { cause: options.cause });
    this.name = "EngineError";
    this.code = options.code;
    this.context = options.context;
  }
}

export function isEngineError(error: unknown): error is EngineError {
  return error instanceof EngineError;
}

type ErrorDetailValue = string | number | boolean | null | undefined;

export type ErrorDetailEntry =
  string | readonly [string, ErrorDetailValue] | null | undefined;

export function errorDetails(
  entries: readonly ErrorDetailEntry[],
): string | undefined {
  const lines = entries
    .map(formatDetailEntry)
    .filter((line): line is string => line !== null && line.length > 0);

  return lines.length > 0 ? lines.join("\n") : undefined;
}

export function detail(key: string, value: ErrorDetailValue): ErrorDetailEntry {
  return value === null || value === undefined ? null : [key, value];
}

export function workspaceIndexDetail(name: string): ErrorDetailEntry {
  return detail("workspaceIndex", name);
}

function formatDetailEntry(entry: ErrorDetailEntry): string | null {
  if (entry === null || entry === undefined) {
    return null;
  }

  if (typeof entry === "string") {
    return entry.trim().length > 0 ? entry.trim() : null;
  }

  const [key, value] = entry;
  if (value === null || value === undefined) {
    return null;
  }

  return `${key}=${String(value)}`;
}
