import type { ChunkOptions } from "../../extraction/index.js";

const DEFAULT_CHARS_PER_100_TOKENS = 185;
const TOKEN_DENSE_CHARS_PER_100_TOKENS = 100;
const TOKEN_DENSE_WINDOW_CHARS = 16 * 1024;
const TOKEN_DENSE_WINDOW_STEP_CHARS = 8 * 1024;
const TOKEN_DENSE_PERCENT = 30;
const CHUNK_OVERLAP_PERCENT = 15;

export function indexChunkOptions(
  maxInputTokens: number | undefined,
  text?: string,
): ChunkOptions {
  if (maxInputTokens === undefined) {
    return {};
  }

  const charsPer100Tokens = isTokenDenseText(text, maxInputTokens)
    ? TOKEN_DENSE_CHARS_PER_100_TOKENS
    : DEFAULT_CHARS_PER_100_TOKENS;
  const maxChunkChars = Math.floor((maxInputTokens * charsPer100Tokens) / 100);
  const chunkOverlapChars = Math.floor(
    (maxChunkChars * CHUNK_OVERLAP_PERCENT) / 100,
  );
  return { maxChunkChars, chunkOverlapChars };
}

function isTokenDenseText(
  text: string | undefined,
  maxInputTokens: number,
): boolean {
  if (
    text === undefined ||
    text.length <= (maxInputTokens * TOKEN_DENSE_CHARS_PER_100_TOKENS) / 100
  ) {
    return false;
  }

  if (text.length <= TOKEN_DENSE_WINDOW_CHARS) {
    return isTokenDenseWindow(text, 0, text.length);
  }

  const lastWindowStart = text.length - TOKEN_DENSE_WINDOW_CHARS;
  for (
    let start = 0;
    start <= lastWindowStart;
    start += TOKEN_DENSE_WINDOW_STEP_CHARS
  ) {
    if (isTokenDenseWindow(text, start, TOKEN_DENSE_WINDOW_CHARS)) {
      return true;
    }
  }

  return (
    lastWindowStart % TOKEN_DENSE_WINDOW_STEP_CHARS !== 0 &&
    isTokenDenseWindow(text, lastWindowStart, TOKEN_DENSE_WINDOW_CHARS)
  );
}

function isTokenDenseWindow(
  text: string,
  start: number,
  length: number,
): boolean {
  let denseChars = 0;
  const end = start + length;
  const requiredDenseChars = Math.ceil((length * TOKEN_DENSE_PERCENT) / 100);

  for (let index = start; index < end; index++) {
    const code = text.charCodeAt(index);
    const asciiLetter =
      (code >= 65 && code <= 90) || (code >= 97 && code <= 122);
    if (!asciiLetter && code !== 32 && code !== 9) {
      denseChars++;
      if (denseChars >= requiredDenseChars) {
        return true;
      }
    }
  }

  return false;
}
