import { EngineError } from "../../errors/index.js";
import type { EntityFragment, TextRange } from "../../types.js";
import { makeEntityId } from "../ids.js";
import { validateSourceFile, type Source, type TextSource } from "../source.js";
import type { ChunkOptions } from "../types.js";

const DEFAULT_TEXT_CHUNK_CHARS = 3600;

const DEFAULT_TEXT_CHUNK_OVERLAP_CHARS = 540;

export class TextExtractor {
  async extract(
    source: Source,
    options: ChunkOptions = {},
  ): Promise<EntityFragment[]> {
    if (source.kind !== "text") {
      throw new EngineError("Text extractor received a non-text source", {
        code: "ZVEC_GREP.ENGINE.EXTRACTORS.TEXT_UNSUPPORTED_SOURCE",
        context: `fileId=${source.file.id} sourceKind=${source.kind}`,
      });
    }

    validateSourceFile(source);
    const { maxChunkChars, chunkOverlapChars } =
      resolveTextChunkOptions(options);

    return extractPlainTextFragments(source, maxChunkChars, chunkOverlapChars);
  }
}

function resolveTextChunkOptions(
  options: ChunkOptions,
): Required<ChunkOptions> {
  const maxChunkChars = options.maxChunkChars ?? DEFAULT_TEXT_CHUNK_CHARS;
  const chunkOverlapChars =
    options.chunkOverlapChars ?? DEFAULT_TEXT_CHUNK_OVERLAP_CHARS;

  if (!Number.isInteger(maxChunkChars) || maxChunkChars <= 0) {
    throw new EngineError(
      "Text extractor requires a positive integer chunk size",
      {
        code: "ZVEC_GREP.ENGINE.EXTRACTORS.TEXT_INVALID_CHUNK_SIZE",
        context: `maxChunkChars=${maxChunkChars}`,
      },
    );
  }

  if (
    !Number.isInteger(chunkOverlapChars) ||
    chunkOverlapChars < 0 ||
    chunkOverlapChars >= maxChunkChars
  ) {
    throw new EngineError(
      "Text extractor requires overlap to be smaller than chunk size",
      {
        code: "ZVEC_GREP.ENGINE.EXTRACTORS.TEXT_INVALID_CHUNK_OVERLAP",
        context: `maxChunkChars=${maxChunkChars} chunkOverlapChars=${chunkOverlapChars}`,
      },
    );
  }

  return { maxChunkChars, chunkOverlapChars };
}

export function extractPlainTextFragments(
  source: TextSource,
  maxChunkChars: number,
  chunkOverlapChars: number,
): EntityFragment[] {
  return chunkText(source.text, maxChunkChars, chunkOverlapChars).map(
    (chunk, index): EntityFragment => ({
      id: makeEntityId(source.file.id, index),
      fileId: source.file.id,
      range: chunk.range,
      content: {
        kind: "text",
        text: chunk.text,
      },
    }),
  );
}

type TextChunk = {
  text: string;
  range: TextRange;
};

function chunkText(
  text: string,
  maxChars: number,
  overlapChars: number,
): TextChunk[] {
  if (text.trim().length === 0) {
    return [];
  }

  const lines = text.split("\n");
  const lineOffsets = computeLineOffsets(lines);
  const chunks: TextChunk[] = [];

  let startIndex = 0;

  while (startIndex < lines.length) {
    if (lines[startIndex].length + 1 > maxChars) {
      const line = lines[startIndex];
      const lineNumber = startIndex + 1;
      let offset = 0;

      while (offset < line.length) {
        const remaining = line.length - offset;
        const sliceLength =
          remaining <= maxChars
            ? remaining
            : findLineCut(line.slice(offset), maxChars);
        const slice = line.slice(offset, offset + sliceLength);

        if (slice.trim().length > 0) {
          chunks.push({
            text: slice,
            range: {
              kind: "text",
              startLine: lineNumber,
              endLine: lineNumber,
              startOffset: lineOffsets[startIndex] + offset,
              endOffset: lineOffsets[startIndex] + offset + slice.length,
            },
          });
        }

        offset += sliceLength;
      }

      startIndex++;
      continue;
    }

    let usedChars = 0;
    let endIndex = startIndex;

    while (endIndex < lines.length) {
      const lineLength = lines[endIndex].length + 1;

      if (usedChars + lineLength > maxChars && endIndex > startIndex) {
        break;
      }

      usedChars += lineLength;
      endIndex++;
    }

    const chunkLines = lines.slice(startIndex, endIndex);
    const chunk = chunkLines.join("\n");

    if (chunk.trim().length > 0) {
      const endLineIndex = endIndex - 1;

      chunks.push({
        text: chunk,
        range: {
          kind: "text",
          startLine: startIndex + 1,
          endLine: endIndex,
          startOffset: lineOffsets[startIndex],
          endOffset: lineOffsets[endLineIndex] + lines[endLineIndex].length,
        },
      });
    }

    if (endIndex >= lines.length) {
      break;
    }

    startIndex = computeNextStartLine(
      lines,
      startIndex,
      endIndex,
      overlapChars,
    );
  }

  return chunks;
}

function computeLineOffsets(lines: readonly string[]): number[] {
  const offsets: number[] = [];
  let offset = 0;

  for (const line of lines) {
    offsets.push(offset);
    offset += line.length + 1;
  }

  return offsets;
}

function computeNextStartLine(
  lines: readonly string[],
  startIndex: number,
  endIndex: number,
  overlapChars: number,
): number {
  if (overlapChars === 0) {
    return endIndex;
  }

  let overlapLines = 0;
  let overlapCount = 0;

  for (
    let index = endIndex - 1;
    index >= startIndex && overlapCount < overlapChars;
    index--
  ) {
    overlapCount += lines[index].length + 1;
    overlapLines++;
  }

  const nextStart = endIndex - overlapLines;

  return nextStart > startIndex ? nextStart : endIndex;
}

function findLineCut(line: string, maxChars: number): number {
  if (line.length <= maxChars) {
    return line.length;
  }

  const minPosition = Math.floor(maxChars * 0.7);
  let bestPosition = -1;
  let bestScore = 0;

  for (let index = minPosition; index < maxChars; index++) {
    const character = line[index];
    let score = 0;

    if (character === "." || character === "!" || character === "?") {
      score = 4;
    } else if (character === "," || character === ";" || character === ":") {
      score = 3;
    } else if (character === " " || character === "\t") {
      score = 2;
    } else if (character === "-" || character === "/" || character === "\\") {
      score = 1;
    }

    if (score > 0 && score >= bestScore) {
      bestScore = score;
      bestPosition = index + 1;
    }
  }

  return bestPosition > 0 ? bestPosition : maxChars;
}
