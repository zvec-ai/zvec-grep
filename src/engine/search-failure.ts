import { isEngineError, type EngineErrorCode } from "./errors.js";

/** Model availability/output failures, never invalid inputs or authorization. */
export function recoverableSearchFailureCode(
  error: unknown,
): EngineErrorCode | undefined {
  if (!isEngineError(error) || !isRecoverableSearchFailureCode(error.code))
    return undefined;
  // Some model backends wrap cancellation in EMBED_FAILED. Do not turn a
  // cancelled request into a successful lexical fallback, even across MCP.
  const seen = new Set<Error>();
  let cause: unknown = error;
  while (cause instanceof Error) {
    if (seen.has(cause) || seen.size >= 16) return undefined;
    seen.add(cause);
    if (
      cause.name === "AbortError" ||
      /\b(?:cancelled|canceled|aborted)\b/i.test(cause.message)
    )
      return undefined;
    if (isEngineError(cause)) {
      // Wrapping an authorization/input failure in EMBED_FAILED must not
      // widen recovery either. Only recognized availability causes qualify.
      if (!isRecoverableSearchFailureCode(cause.code)) return undefined;
      if (cause.code.endsWith("_API_ERROR")) {
        const status = searchFailureHttpStatus(cause);
        // The backend puts its HTTP status before provider-supplied text.
        if (status !== 408 && status !== 429 && !(status && status >= 500))
          return undefined;
      }
    }
    cause = cause.cause;
  }
  return error.code;
}

export function searchFailureHttpStatus(error: unknown): number | undefined {
  if (!isEngineError(error) || !error.code.endsWith("_API_ERROR"))
    return undefined;
  const value = error.context?.match(/(?:^|\s)status=(\d{3})(?=\s|$)/)?.[1];
  const status = value === undefined ? undefined : Number(value);
  return status !== undefined && status >= 100 && status <= 599
    ? status
    : undefined;
}

export function isRecoverableSearchFailureCode(
  code: unknown,
): code is EngineErrorCode {
  if (typeof code !== "string") return false;
  const prefix = "ZVEC_GREP.ENGINE.MODELS.";
  if (!code.startsWith(prefix)) return false;
  const name = code.slice(prefix.length);
  return (
    /^(?:MODEL2VEC_(?:EMBED|LOAD|DOWNLOAD)_FAILED|TRANSFORMERS_JS_(?:EMBED_FAILED|TOKENIZATION_FAILED|MISSING_DEPENDENCY|INVALID_TENSOR)|LLAMA_CPP_(?:EMBED_FAILED|MISSING_DEPENDENCY|INVALID_GGUF(?:_HTML)?))$/.test(
      name,
    ) ||
    /^QWEN(?:_TEXT_EMBEDDING_V4|37_TEXT_EMBEDDING|3_VL_EMBEDDING)_(?:REQUEST_FAILED|API_ERROR|INVALID_JSON|MISSING_DATA|MISSING_EMBEDDINGS|INVALID_INDEX|INDEX_OUT_OF_RANGE|INVALID_VECTOR|INVALID_ITEM)$/.test(
      name,
    ) ||
    /^EMBEDDING_(?:INVALID_RESPONSE|VECTOR_COUNT_MISMATCH|INVALID_VECTOR|DIMENSION_MISMATCH|NON_FINITE_VECTOR_VALUE|INVALID_TRUNCATION|INVALID_TRUNCATED_INPUT_INDEX)$/.test(
      name,
    )
  );
}
