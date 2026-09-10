import { EngineError, redactErrorText } from "../engine/errors.js";
import {
  isRecoverableSearchFailureCode,
  recoverableSearchFailureCode,
  searchFailureHttpStatus,
} from "../engine/search-failure.js";

const SEARCH_ERROR_CODE_META_KEY = "io.zvec-grep/search-error-code";
const SEARCH_HTTP_STATUS_META_KEY = "io.zvec-grep/search-http-status";

/** Preserve a machine-readable failure category without provider details. */
export function searchFailureResult(error: unknown) {
  const code = recoverableSearchFailureCode(error);
  if (!code) return undefined;
  const status = searchFailureHttpStatus(error);
  return {
    isError: true as const,
    content: [
      {
        type: "text" as const,
        text: redactErrorText((error as EngineError).message, 512),
      },
    ],
    _meta: {
      [SEARCH_ERROR_CODE_META_KEY]: code,
      ...(status === undefined
        ? {}
        : { [SEARCH_HTTP_STATUS_META_KEY]: status }),
    },
  };
}

export function searchFailureFromMeta(
  message: string,
  meta: unknown,
): EngineError | undefined {
  if (typeof meta !== "object" || meta === null) return undefined;
  const fields = meta as Record<string, unknown>;
  const code = fields[SEARCH_ERROR_CODE_META_KEY];
  if (!isRecoverableSearchFailureCode(code)) return undefined;
  const status = fields[SEARCH_HTTP_STATUS_META_KEY];
  const error = new EngineError(message, {
    code,
    context:
      typeof status === "number" && Number.isInteger(status)
        ? `status=${status}`
        : undefined,
  });
  return recoverableSearchFailureCode(error) ? error : undefined;
}
