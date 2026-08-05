import { EngineError } from "../../errors/index.js";
import type {
  EntityFragment,
  MarkdownEntityMetadata,
  TextRange,
} from "../../types.js";
import { makeEntityId } from "../ids.js";
import { validateSourceFile, type Source, type TextSource } from "../source.js";
import { extractPlainTextFragments } from "../text/extractor.js";
import type { ChunkOptions } from "../types.js";

const DEFAULT_MARKDOWN_CHUNK_CHARS = 3600;
const DEFAULT_MARKDOWN_CHUNK_OVERLAP_CHARS = 540;

type Heading = {
  level: number;
  text: string;
  lineIndex: number;
};

type Section = {
  heading: Heading | null;
  startIndex: number;
  endIndex: number;
  breadcrumb: readonly string[];
};

type MarkdownWindow = {
  text: string;
  range: TextRange;
};

export class MarkdownExtractor {
  async extract(
    source: Source,
    options: ChunkOptions = {},
  ): Promise<EntityFragment[]> {
    if (source.kind !== "text" || source.file.format !== "markdown") {
      return [];
    }

    validateSourceFile(source);
    const chunkOptions = resolveMarkdownChunkOptions(options);

    const lines = source.text.split("\n");
    const headings = scanHeadings(lines);
    if (headings.length === 0) {
      return this.fallback(source, chunkOptions);
    }

    const lineOffsets = computeLineOffsets(lines);
    const fenceLines = computeFenceLines(lines);
    const sections = buildSections(headings, lines);
    const fragments: EntityFragment[] = [];

    for (const section of sections) {
      const metadata = markdownMetadata(section);
      const windows = splitMarkdownSection({
        lines,
        lineOffsets,
        fenceLines,
        section,
        maxChars: chunkOptions.maxChunkChars,
        overlapChars: chunkOptions.chunkOverlapChars,
      });

      if (windows.length > 1) {
        const id = makeEntityId(source.file.id, fragments.length);
        fragments.push({
          id,
          group: id,
          fileId: source.file.id,
          range: linesToWindow(
            lines,
            lineOffsets,
            section.startIndex,
            section.endIndex,
          ).range,
          content: {
            kind: "text",
            text: markdownOutline(metadata),
          },
          metadata,
        });

        for (const window of windows) {
          fragments.push(
            markdownWindowToFragment(
              source,
              metadata,
              window,
              fragments.length,
              id,
            ),
          );
        }

        continue;
      }

      for (const window of windows) {
        fragments.push({
          id: makeEntityId(source.file.id, fragments.length),
          fileId: source.file.id,
          range: window.range,
          content: {
            kind: "text",
            text: window.text,
          },
          metadata,
        });
      }
    }

    return fragments.length > 0
      ? fragments
      : this.fallback(source, chunkOptions);
  }

  private fallback(
    source: TextSource,
    options: Required<ChunkOptions>,
  ): EntityFragment[] {
    return extractPlainTextFragments(
      source,
      options.maxChunkChars,
      options.chunkOverlapChars,
    );
  }
}

function resolveMarkdownChunkOptions(
  options: ChunkOptions,
): Required<ChunkOptions> {
  const maxChunkChars = options.maxChunkChars ?? DEFAULT_MARKDOWN_CHUNK_CHARS;
  const chunkOverlapChars =
    options.chunkOverlapChars ?? DEFAULT_MARKDOWN_CHUNK_OVERLAP_CHARS;

  if (!Number.isInteger(maxChunkChars) || maxChunkChars <= 0) {
    throw new EngineError(
      "Markdown extractor requires a positive integer chunk size",
      {
        code: "ZVEC_GREP.ENGINE.EXTRACTORS.MARKDOWN_INVALID_CHUNK_SIZE",
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
      "Markdown extractor requires overlap to be smaller than chunk size",
      {
        code: "ZVEC_GREP.ENGINE.EXTRACTORS.MARKDOWN_INVALID_CHUNK_OVERLAP",
        context: `maxChunkChars=${maxChunkChars} chunkOverlapChars=${chunkOverlapChars}`,
      },
    );
  }

  return { maxChunkChars, chunkOverlapChars };
}

function markdownWindowToFragment(
  source: TextSource,
  metadata: MarkdownEntityMetadata,
  window: MarkdownWindow,
  index: number,
  group: string,
): EntityFragment {
  return {
    id: makeEntityId(source.file.id, index),
    group,
    fileId: source.file.id,
    range: window.range,
    content: {
      kind: "text",
      text: window.text,
    },
    metadata,
  };
}

function markdownOutline(metadata: MarkdownEntityMetadata): string {
  return metadata.heading ?? "markdown section";
}

function scanHeadings(lines: readonly string[]): Heading[] {
  const headings: Heading[] = [];
  let fence: "```" | "~~~" | null = null;

  for (let index = 0; index < lines.length; index++) {
    const line = lines[index];
    const trimmed = line.trimStart();

    if (fence) {
      if (trimmed.startsWith(fence)) {
        fence = null;
      }
      continue;
    }

    if (trimmed.startsWith("```")) {
      fence = "```";
      continue;
    }

    if (trimmed.startsWith("~~~")) {
      fence = "~~~";
      continue;
    }

    const atx = line.match(/^(#{1,6})\s+(.+?)\s*#*\s*$/);
    if (atx) {
      headings.push({
        level: atx[1].length,
        text: atx[2].trim(),
        lineIndex: index,
      });
      continue;
    }

    const next = lines[index + 1]?.trim();
    if (line.trim().length > 0 && next && /^(=+|-+)\s*$/.test(next)) {
      headings.push({
        level: next.startsWith("=") ? 1 : 2,
        text: line.trim(),
        lineIndex: index,
      });
      index++;
    }
  }

  return headings;
}

function buildSections(
  headings: readonly Heading[],
  lines: readonly string[],
): Section[] {
  const stack: Heading[] = [];
  const sections: Section[] = [];
  const firstHeading = headings[0];

  if (
    firstHeading.lineIndex > 0 &&
    lines.slice(0, firstHeading.lineIndex).join("\n").trim().length > 0
  ) {
    sections.push({
      heading: null,
      startIndex: 0,
      endIndex: firstHeading.lineIndex - 1,
      breadcrumb: [],
    });
  }

  for (let index = 0; index < headings.length; index++) {
    const heading = headings[index];

    while (stack.length > 0 && stack[stack.length - 1].level >= heading.level) {
      stack.pop();
    }

    sections.push({
      heading,
      startIndex: heading.lineIndex,
      endIndex:
        index + 1 < headings.length
          ? headings[index + 1].lineIndex - 1
          : lines.length - 1,
      breadcrumb: stack.map((item) => item.text),
    });
    stack.push(heading);
  }

  return sections;
}

function splitMarkdownSection(input: {
  lines: readonly string[];
  lineOffsets: readonly number[];
  fenceLines: readonly boolean[];
  section: Section;
  maxChars: number;
  overlapChars: number;
}): MarkdownWindow[] {
  const windows: MarkdownWindow[] = [];
  let startIndex = input.section.startIndex;

  while (startIndex <= input.section.endIndex) {
    if (input.lines[startIndex].length + 1 > input.maxChars) {
      windows.push(
        ...splitLongLine(
          input.lines[startIndex],
          startIndex,
          input.lineOffsets[startIndex],
          input.maxChars,
        ),
      );
      startIndex++;
      continue;
    }

    let endIndex = startIndex;
    let usedChars = 0;

    while (endIndex <= input.section.endIndex) {
      const lineLength = input.lines[endIndex].length + 1;
      if (usedChars + lineLength > input.maxChars && endIndex > startIndex) {
        break;
      }
      usedChars += lineLength;
      endIndex++;
    }

    if (endIndex <= input.section.endIndex && endIndex - startIndex > 1) {
      endIndex = chooseMarkdownBreak(
        input.lines,
        input.fenceLines,
        startIndex,
        endIndex,
      );
    }

    windows.push(
      linesToWindow(input.lines, input.lineOffsets, startIndex, endIndex - 1),
    );

    if (endIndex > input.section.endIndex) {
      break;
    }

    const overlapLines = computeMarkdownOverlapLines(
      input.lines,
      startIndex,
      endIndex,
      input.overlapChars,
    );
    const nextStart = endIndex - overlapLines;
    startIndex = nextStart > startIndex ? nextStart : endIndex;
  }

  return windows.filter((window) => window.text.trim().length > 0);
}

function chooseMarkdownBreak(
  lines: readonly string[],
  fenceLines: readonly boolean[],
  startIndex: number,
  endIndex: number,
): number {
  const minBreak =
    startIndex + Math.max(1, Math.floor((endIndex - startIndex) * 0.7));
  let bestBreak = endIndex;
  let bestScore = markdownBreakScore(lines, fenceLines, endIndex);

  for (let index = minBreak; index <= endIndex; index++) {
    const score = markdownBreakScore(lines, fenceLines, index);
    if (score > bestScore) {
      bestBreak = index;
      bestScore = score;
    }
  }

  return bestBreak;
}

function markdownBreakScore(
  lines: readonly string[],
  fenceLines: readonly boolean[],
  breakIndex: number,
): number {
  if (breakIndex <= 0 || breakIndex >= lines.length || fenceLines[breakIndex]) {
    return 0;
  }

  const current = lines[breakIndex].trim();
  const previous = lines[breakIndex - 1].trim();

  if (/^#{1,6}\s+/.test(current)) {
    return 100;
  }

  if (previous === "" && current === "") {
    return 70;
  }

  if (previous === "") {
    return 60;
  }

  if (/^([-*+]|\d+\.)\s+/.test(current)) {
    return 35;
  }

  if (/^>\s+/.test(current)) {
    return 25;
  }

  return 10;
}

function splitLongLine(
  line: string,
  lineIndex: number,
  lineOffset: number,
  maxChars: number,
): MarkdownWindow[] {
  const windows: MarkdownWindow[] = [];
  let offset = 0;

  while (offset < line.length) {
    const remaining = line.length - offset;
    const sliceLength =
      remaining <= maxChars
        ? remaining
        : findLineCut(line.slice(offset), maxChars);
    const text = line.slice(offset, offset + sliceLength);

    windows.push({
      text,
      range: {
        kind: "text",
        startLine: lineIndex + 1,
        endLine: lineIndex + 1,
        startOffset: lineOffset + offset,
        endOffset: lineOffset + offset + text.length,
      },
    });
    offset += sliceLength;
  }

  return windows;
}

function linesToWindow(
  lines: readonly string[],
  lineOffsets: readonly number[],
  startIndex: number,
  endIndex: number,
): MarkdownWindow {
  return {
    text: lines.slice(startIndex, endIndex + 1).join("\n"),
    range: {
      kind: "text",
      startLine: startIndex + 1,
      endLine: endIndex + 1,
      startOffset: lineOffsets[startIndex],
      endOffset: lineOffsets[endIndex] + lines[endIndex].length,
    },
  };
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

function computeFenceLines(lines: readonly string[]): boolean[] {
  const inFence = new Array<boolean>(lines.length).fill(false);
  let fence: "```" | "~~~" | null = null;

  for (let index = 0; index < lines.length; index++) {
    const trimmed = lines[index].trimStart();

    if (fence) {
      inFence[index] = true;
      if (trimmed.startsWith(fence)) {
        fence = null;
      }
      continue;
    }

    if (trimmed.startsWith("```")) {
      fence = "```";
    } else if (trimmed.startsWith("~~~")) {
      fence = "~~~";
    }
  }

  return inFence;
}

function computeMarkdownOverlapLines(
  lines: readonly string[],
  startIndex: number,
  endIndex: number,
  overlapChars: number,
): number {
  if (overlapChars <= 0) {
    return 0;
  }

  let chars = 0;
  let count = 0;

  for (let index = endIndex - 1; index > startIndex; index--) {
    chars += lines[index].length + 1;
    if (chars > overlapChars) {
      break;
    }
    count++;
  }

  return Math.min(count, Math.floor((endIndex - startIndex) / 2));
}

function findLineCut(line: string, maxChars: number): number {
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

    if (score >= bestScore && score > 0) {
      bestScore = score;
      bestPosition = index + 1;
    }
  }

  return bestPosition > 0 ? bestPosition : maxChars;
}

function markdownMetadata(section: Section): MarkdownEntityMetadata {
  return {
    kind: "markdown",
    heading: section.heading?.text ?? null,
    level: section.heading?.level ?? null,
    scope: section.breadcrumb.length > 0 ? section.breadcrumb.join("::") : null,
  };
}
