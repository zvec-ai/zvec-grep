import { open, readFile, stat } from "node:fs/promises";
import { constants } from "node:fs";
import { relative } from "node:path";
import { extract } from "../extraction/index.js";
import type { EntityFragment, FileInfo, Range } from "../types.js";
import type {
  ZvecGrepStructureEnrichmentDiagnostics,
  ZvecGrepContextContainer,
  ZvecGrepContextItem,
} from "./types.js";
import { detectFileType } from "../file-type.js";
import { resolveMaxFileSizeBytes } from "../file-size-policy.js";
import { normalizePath, toDisplayPath } from "../utils/path.js";
import { sha256Text } from "../utils/hash.js";

export const RG_STRUCTURE_ENRICH_FILE_LIMIT = 100;

type StructureEnrichmentResult = {
  items: ZvecGrepContextItem[];
  diagnostics: ZvecGrepStructureEnrichmentDiagnostics;
};

type StructuralSource = {
  fragments: EntityFragment[];
  text: string;
  /** File offsets in UTF-16 code units, counting both CR and LF. */
  lineOffsets: number[];
};

const STRUCTURE_ENRICH_FILE_ID_NAMESPACE = "__rg_structure__";
export async function enrichLexicalItemsWithStructure(
  root: string,
  items: readonly ZvecGrepContextItem[],
  fileLimit = RG_STRUCTURE_ENRICH_FILE_LIMIT,
  maxFileSizeBytes?: number,
  strictSourceWindow = false,
  execution: {
    signal?: AbortSignal;
    /** Verify exact source ownership while retaining the literal source window. */
    preciseSourceOwnership?: boolean;
  } = {},
): Promise<StructureEnrichmentResult> {
  const { signal } = execution;
  const verifySource =
    strictSourceWindow || execution.preciseSourceOwnership === true;
  signal?.throwIfAborted();
  items = [...items];
  const matchedFiles = uniqueLexicalFilePaths(items);
  const selectedFiles = new Set(matchedFiles.slice(0, fileLimit));
  const sourcesByFile = new Map<string, StructuralSource | null>();
  let parsedFiles = 0;

  for (const absolutePath of selectedFiles) {
    signal?.throwIfAborted();
    const source = await parseStructuralFragments(
      root,
      absolutePath,
      maxFileSizeBytes,
      verifySource,
      signal,
    );
    signal?.throwIfAborted();
    sourcesByFile.set(absolutePath, source);
    if (source !== null) {
      parsedFiles++;
    }
  }

  let enrichedItems = 0;
  const enrichedFiles = new Set<string>();
  const enriched = items.map((item) => {
    signal?.throwIfAborted();
    if (item.kind !== "lexical_match") {
      return item;
    }

    const source = sourcesByFile.get(normalizePath(item.file.absolutePath));
    if (!source) {
      return verifySource ? withoutContainer(item) : item;
    }

    const matchRange = lexicalMatchRange(item);
    if (!matchRange) {
      return verifySource ? withoutContainer(item) : item;
    }

    const window = verifySource
      ? verifiedSourceWindow(item, source, matchRange)
      : undefined;
    if (verifySource && !window) {
      // The rg/context read predates this extraction, or its positions cannot
      // be verified. Do not attach a new owner to old source, and do not let a
      // caller's unstructured fallback redisplay that stale match line.
      return {
        ...withoutContainer(item),
        content: "",
        status: "possibly_stale" as const,
      };
    }
    const container = window
      ? smallestContainingSourceFragment(source, window)
      : smallestContainingFragment(source.fragments, matchRange);
    if (!container) {
      return verifySource ? withoutContainer(item) : item;
    }

    enrichedItems++;
    enrichedFiles.add(item.file.absolutePath);

    const contextContainer: ZvecGrepContextContainer = {
      entityId: container.group ?? container.id,
      range: container.range,
      metadata: container.metadata,
    };

    return {
      ...(strictSourceWindow && window
        ? clipSourceWindow(item, source, window, container)
        : item),
      metadata: container.metadata ?? item.metadata,
      container: contextContainer,
    };
  });

  return {
    items: enriched,
    diagnostics: {
      source: "structural_extraction",
      fileLimit,
      matchedFiles: matchedFiles.length,
      parsedFiles,
      enrichedFiles: enrichedFiles.size,
      enrichedItems,
      skippedFiles: Math.max(0, matchedFiles.length - parsedFiles),
      truncated: matchedFiles.length > fileLimit,
    },
  };
}

function uniqueLexicalFilePaths(
  items: readonly ZvecGrepContextItem[],
): string[] {
  const seen = new Set<string>();
  const paths: string[] = [];

  for (const item of items) {
    if (item.kind !== "lexical_match") {
      continue;
    }

    const absolutePath = normalizePath(item.file.absolutePath);
    if (!seen.has(absolutePath)) {
      seen.add(absolutePath);
      paths.push(absolutePath);
    }
  }

  return paths;
}

async function parseStructuralFragments(
  root: string,
  absolutePath: string,
  maxFileSizeBytes?: number,
  retainSource = false,
  signal?: AbortSignal,
): Promise<StructuralSource | null> {
  try {
    signal?.throwIfAborted();
    const file = await fileInfoForStructure(
      root,
      absolutePath,
      maxFileSizeBytes,
    );
    signal?.throwIfAborted();
    if (!file) {
      return null;
    }

    const text = retainSource
      ? await readBoundedStructuralSource(
          absolutePath,
          resolveMaxFileSizeBytes(file.kind, maxFileSizeBytes),
          signal,
        )
      : await readFile(absolutePath, { encoding: "utf8", signal });
    signal?.throwIfAborted();
    if (text === null) return null;
    const source = {
      kind: "text",
      file,
      text,
    } as const;
    const fragments = await extract(source);
    signal?.throwIfAborted();
    const structuralFragments = fragments.filter(isStructuralFragment);
    if (structuralFragments.length === 0) return null;
    const lineOffsets = retainSource ? [0] : [];
    if (retainSource) {
      for (let index = 0; index < text.length; index++) {
        if (text[index] === "\n") lineOffsets.push(index + 1);
      }
    }
    return {
      fragments: structuralFragments,
      text: retainSource ? text : "",
      lineOffsets,
    };
  } catch {
    signal?.throwIfAborted();
    return null;
  }
}

async function readBoundedStructuralSource(
  absolutePath: string,
  maxBytes: number,
  signal?: AbortSignal,
): Promise<string | null> {
  signal?.throwIfAborted();
  if (!Number.isSafeInteger(maxBytes) || maxBytes < 0) return null;
  const handle = await open(
    absolutePath,
    constants.O_RDONLY | constants.O_NONBLOCK,
  );
  try {
    signal?.throwIfAborted();
    const info = await handle.stat();
    signal?.throwIfAborted();
    if (!info.isFile() || info.size > maxBytes) return null;
    // A file can grow after either stat. Never ask readFile to allocate from
    // its changing size: one bounded descriptor read has a one-byte sentinel.
    const buffer = Buffer.allocUnsafe(maxBytes + 1);
    let size = 0;
    while (size < buffer.length) {
      signal?.throwIfAborted();
      const { bytesRead } = await handle.read(
        buffer,
        size,
        buffer.length - size,
        size,
      );
      signal?.throwIfAborted();
      if (bytesRead === 0) break;
      size += bytesRead;
    }
    return size > maxBytes ? null : buffer.toString("utf8", 0, size);
  } finally {
    await handle.close();
  }
}

function withoutContainer(item: ZvecGrepContextItem): ZvecGrepContextItem {
  return { ...item, container: undefined, metadata: undefined };
}

type SourceWindow = {
  startOffset: number;
  endOffset: number;
  matchStartOffset: number;
  matchEndOffset: number;
};

function lineEndOffset(source: StructuralSource, lineIndex: number): number {
  const nextLine = source.lineOffsets[lineIndex + 1];
  if (nextLine === undefined) return source.text.length;
  const beforeNewline = nextLine - 1;
  return source.text[beforeNewline - 1] === "\r"
    ? beforeNewline - 1
    : beforeNewline;
}

function verifiedSourceWindow(
  item: ZvecGrepContextItem,
  source: StructuralSource,
  match: Extract<Range, { kind: "text" }>,
): SourceWindow | null {
  if (item.range.kind !== "text") return null;
  const { startLine, endLine } = item.range;
  if (
    !Number.isSafeInteger(startLine) ||
    !Number.isSafeInteger(endLine) ||
    startLine < 1 ||
    endLine < startLine ||
    endLine > source.lineOffsets.length ||
    match.startLine < startLine ||
    match.endLine > endLine ||
    match.endLine < match.startLine
  ) {
    return null;
  }
  const startOffset = source.lineOffsets[startLine - 1]!;
  const endOffset = lineEndOffset(source, endLine - 1);
  // rg's expanded context normalizes CRLF to LF. Compare complete source
  // lines before cropping, rather than trusting an old match's line numbers.
  if (
    source.text.slice(startOffset, endOffset).replace(/\r\n/g, "\n") !==
    item.content.replace(/\r\n/g, "\n")
  ) {
    return null;
  }
  const absoluteMatchOffset = (line: number, column: number): number | null => {
    const lineOffset = source.lineOffsets[line - 1];
    if (
      lineOffset === undefined ||
      !Number.isSafeInteger(column) ||
      column < 0 ||
      column > lineEndOffset(source, line - 1) - lineOffset
    ) {
      return null;
    }
    // parseRipgrepJsonLine already converts UTF-8 byte offsets to UTF-16
    // line columns. The extractor's start/end offsets are UTF-16 file offsets.
    return lineOffset + column;
  };
  const matchStartOffset = absoluteMatchOffset(
    match.startLine,
    match.startOffset,
  );
  const matchEndOffset = absoluteMatchOffset(match.endLine, match.endOffset);
  if (
    matchStartOffset === null ||
    matchEndOffset === null ||
    matchEndOffset <= matchStartOffset
  ) {
    return null;
  }
  return { startOffset, endOffset, matchStartOffset, matchEndOffset };
}

function smallestContainingSourceFragment(
  source: StructuralSource,
  window: SourceWindow,
): EntityFragment | null {
  let best: EntityFragment | null = null;
  for (const fragment of source.fragments) {
    const range = fragment.range;
    if (
      range.kind !== "text" ||
      !Number.isSafeInteger(range.startOffset) ||
      !Number.isSafeInteger(range.endOffset) ||
      range.startOffset < 0 ||
      range.endOffset > source.text.length ||
      range.endOffset <= range.startOffset ||
      range.startOffset > window.matchStartOffset ||
      range.endOffset < window.matchEndOffset
    ) {
      continue;
    }
    const bestRange = best?.range;
    if (
      !best ||
      bestRange?.kind !== "text" ||
      range.endOffset - range.startOffset <
        bestRange.endOffset - bestRange.startOffset ||
      (range.endOffset - range.startOffset ===
        bestRange.endOffset - bestRange.startOffset &&
        compareFragmentContainer(fragment, best) < 0)
    ) {
      best = fragment;
    }
  }
  return best;
}

function clipSourceWindow(
  item: ZvecGrepContextItem,
  source: StructuralSource,
  window: SourceWindow,
  container: EntityFragment,
): ZvecGrepContextItem {
  if (container.range.kind !== "text") return item;
  const startOffset = Math.max(window.startOffset, container.range.startOffset);
  const endOffset = Math.min(window.endOffset, container.range.endOffset);
  const lineAt = (offset: number): number => {
    let low = 0;
    let high = source.lineOffsets.length;
    while (low + 1 < high) {
      const middle = Math.floor((low + high) / 2);
      if (source.lineOffsets[middle]! <= offset) low = middle;
      else high = middle;
    }
    return low + 1;
  };
  const startLine = lineAt(startOffset);
  const endLine = lineAt(endOffset);
  return {
    ...item,
    content: source.text.slice(startOffset, endOffset).replace(/\r\n/g, "\n"),
    range: {
      kind: "text",
      startLine,
      endLine,
      startOffset: startOffset - source.lineOffsets[startLine - 1]!,
      endOffset: endOffset - source.lineOffsets[endLine - 1]!,
    },
  };
}

function isStructuralFragment(fragment: EntityFragment): boolean {
  return (
    fragment.metadata?.kind === "code" || fragment.metadata?.kind === "markdown"
  );
}

async function fileInfoForStructure(
  root: string,
  absolutePath: string,
  maxFileSizeBytes?: number,
): Promise<FileInfo | null> {
  const info = await stat(absolutePath).catch(() => null);
  if (!info || !info.isFile() || info.size <= 0) {
    return null;
  }

  const detected = detectFileType(absolutePath);
  if (!detected || !isStructurallyEnrichableFile(detected)) {
    return null;
  }
  if (info.size > resolveMaxFileSizeBytes(detected.kind, maxFileSizeBytes)) {
    return null;
  }

  return {
    id: makeStructureFileId(absolutePath),
    absolutePath: normalizePath(absolutePath),
    relativePath: toDisplayPath(relative(root, absolutePath) || "."),
    rootPath: root,
    sizeBytes: info.size,
    lastModifiedTime: info.mtimeMs,
    kind: detected.kind,
    format: detected.format,
  };
}

function isStructurallyEnrichableFile(
  file: Pick<FileInfo, "kind" | "format">,
): boolean {
  return (
    file.kind === "code" || (file.kind === "text" && file.format === "markdown")
  );
}

function lexicalMatchRange(
  item: ZvecGrepContextItem,
): Extract<Range, { kind: "text" }> | null {
  const range = item.excerptRange ?? item.range;
  return range.kind === "text" ? range : null;
}

function smallestContainingFragment(
  fragments: readonly EntityFragment[],
  matchRange: Extract<Range, { kind: "text" }>,
): EntityFragment | null {
  let best: EntityFragment | null = null;

  for (const fragment of fragments) {
    if (!textRangeContains(fragment.range, matchRange)) {
      continue;
    }

    if (!best || compareFragmentContainer(fragment, best) < 0) {
      best = fragment;
    }
  }

  return best;
}

function textRangeContains(
  outer: Range,
  inner: Extract<Range, { kind: "text" }>,
): boolean {
  return (
    outer.kind === "text" &&
    outer.startLine <= inner.startLine &&
    outer.endLine >= inner.endLine
  );
}

function compareFragmentContainer(
  left: EntityFragment,
  right: EntityFragment,
): number {
  const leftRange = left.range;
  const rightRange = right.range;
  const leftLines =
    leftRange.kind === "text"
      ? leftRange.endLine - leftRange.startLine
      : Number.MAX_SAFE_INTEGER;
  const rightLines =
    rightRange.kind === "text"
      ? rightRange.endLine - rightRange.startLine
      : Number.MAX_SAFE_INTEGER;

  return (
    leftLines - rightLines ||
    fragmentSpecificityScore(right) - fragmentSpecificityScore(left) ||
    left.id.localeCompare(right.id)
  );
}

function fragmentSpecificityScore(fragment: EntityFragment): number {
  const metadata = fragment.metadata;
  if (!metadata) {
    return 0;
  }

  if (metadata.kind === "code") {
    return metadata.symbolName ? 2 : 1;
  }

  return metadata.heading ? 1 : 0;
}

function makeStructureFileId(absolutePath: string): string {
  return sha256Text(
    `${STRUCTURE_ENRICH_FILE_ID_NAMESPACE}\0${normalizePath(absolutePath)}`,
  );
}
