import { EngineError } from "../errors/index.js";
import type { EntityFragment, TextRange } from "../types.js";
import { makeEntityId } from "./ids.js";
import { CodeExtractor } from "./code/extractor.js";
import { MarkdownExtractor } from "./markdown/extractor.js";
import { validateSourceFile, type Source } from "./source.js";

export type { ImageSource, Source, SourceKind, TextSource } from "./source.js";

export interface Extractor {
  supports(source: Source): boolean;
  extract(source: Source): Promise<EntityFragment[]>;
}

export type ChunkOptions = {
  maxChunkChars?: number;
  chunkOverlapChars?: number;
};

const DEFAULT_TEXT_CHUNK_CHARS = 3600;

const DEFAULT_TEXT_CHUNK_OVERLAP_CHARS = 540;

export class TextExtractor implements Extractor {
  private readonly maxChunkChars: number;
  private readonly chunkOverlapChars: number;

  constructor(options: ChunkOptions = {}) {
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

    this.maxChunkChars = maxChunkChars;
    this.chunkOverlapChars = chunkOverlapChars;
  }

  supports(source: Source): boolean {
    return source.kind === "text";
  }

  async extract(source: Source): Promise<EntityFragment[]> {
    if (source.kind !== "text") {
      throw new EngineError("Text extractor received a non-text source", {
        code: "ZVEC_GREP.ENGINE.EXTRACTORS.TEXT_UNSUPPORTED_SOURCE",
        context: `fileId=${source.file.id} sourceKind=${source.kind}`,
      });
    }

    validateSourceFile(source);

    const chunks = chunkText(
      source.text,
      this.maxChunkChars,
      this.chunkOverlapChars,
    );

    return chunks.map((chunk, index): EntityFragment => ({
      id: makeEntityId(source.file.id, index),
      fileId: source.file.id,
      range: chunk.range,
      content: {
        kind: "text",
        text: chunk.text,
      },
    }));
  }
}

export class ImageExtractor implements Extractor {
  supports(source: Source): boolean {
    return source.kind === "image";
  }

  async extract(source: Source): Promise<EntityFragment[]> {
    if (source.kind !== "image") {
      throw new EngineError("Image extractor received a non-image source", {
        code: "ZVEC_GREP.ENGINE.EXTRACTORS.IMAGE_UNSUPPORTED_SOURCE",
        context: `fileId=${source.file.id} sourceKind=${source.kind}`,
      });
    }

    validateSourceFile(source);

    if (source.data.byteLength === 0) {
      throw new EngineError("Image extractor requires non-empty image data", {
        code: "ZVEC_GREP.ENGINE.EXTRACTORS.IMAGE_EMPTY_DATA",
        context: `fileId=${source.file.id} format=${source.format}`,
      });
    }

    return [
      {
        id: makeEntityId(source.file.id, 0),
        fileId: source.file.id,
        range: {
          kind: "file",
        },
        content: {
          kind: "image",
          data: source.data,
          format: source.format,
        },
      },
    ];
  }
}

function createDefaultExtractors(
  options: ChunkOptions = {},
): readonly Extractor[] {
  return [
    new CodeExtractor(options),
    new MarkdownExtractor(options),
    new TextExtractor(options),
    new ImageExtractor(),
  ];
}

export class ExtractorRegistry {
  private readonly extractors: readonly Extractor[];

  constructor(extractors: readonly Extractor[] = createDefaultExtractors()) {
    if (extractors.length === 0) {
      throw new EngineError(
        "Extractor registry requires at least one extractor",
        {
          code: "ZVEC_GREP.ENGINE.EXTRACTORS.EMPTY_REGISTRY",
        },
      );
    }

    this.extractors = extractors;
  }

  resolve(source: Source): Extractor {
    for (const extractor of this.extractors) {
      if (extractor.supports(source)) {
        return extractor;
      }
    }

    throw new EngineError("No extractor supports source", {
      code: "ZVEC_GREP.ENGINE.EXTRACTORS.UNSUPPORTED_SOURCE",
      context: `fileId=${source.file.id} sourceKind=${source.kind}`,
    });
  }

  extract(source: Source): Promise<EntityFragment[]> {
    return this.extractFirst(source);
  }

  private async extractFirst(source: Source): Promise<EntityFragment[]> {
    let matched = false;
    let lastResult: EntityFragment[] = [];

    for (const extractor of this.extractors) {
      if (!extractor.supports(source)) {
        continue;
      }

      matched = true;
      lastResult = await extractor.extract(source);

      if (lastResult.length > 0) {
        return lastResult;
      }
    }

    if (matched) {
      return lastResult;
    }

    throw new EngineError("No extractor supports source", {
      code: "ZVEC_GREP.ENGINE.EXTRACTORS.UNSUPPORTED_SOURCE",
      context: `fileId=${source.file.id} sourceKind=${source.kind}`,
    });
  }
}

export function createDefaultExtractorRegistry(
  options: ChunkOptions = {},
): ExtractorRegistry {
  return new ExtractorRegistry(createDefaultExtractors(options));
}

export function extractFragments(source: Source): Promise<EntityFragment[]> {
  return new ExtractorRegistry().extract(source);
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
