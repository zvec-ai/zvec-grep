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

export {
  collectionDetail,
  detail,
  errorDetails,
  type ErrorDetailEntry,
} from "./details.js";
