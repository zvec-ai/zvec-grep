import type { Range } from "../types.js";
import { normalizePath } from "../utils/path.js";
import type {
  ZvecGrepContextItem,
  ZvecGrepLexicalOccurrence,
} from "./types.js";

export type RgCompactionDiagnostics = {
  rawOccurrences: number;
  uniqueOccurrences: number;
  exactDuplicatesRemoved: number;
  groupsFound: number;
  groupsReturned: number;
  occurrencesCollapsed: number;
  contextWindowsMerged: number;
  groupTruncated: boolean;
};

export type RgCompactionResult = {
  items: ZvecGrepContextItem[];
  diagnostics: RgCompactionDiagnostics;
};

type IndexedContextItem = {
  item: ZvecGrepContextItem;
  index: number;
};

type ContextItemGroup = {
  firstIndex: number;
  entries: IndexedContextItem[];
};

export const MAX_RG_GROUP_OCCURRENCE_SAMPLES = 8;

export function compactRgContextItems(options: {
  items: readonly ZvecGrepContextItem[];
  limit?: number;
  rawOccurrences?: number;
}): RgCompactionResult {
  const unique = dedupeExactOccurrences(options.items);
  const groups = groupContextItems(unique);
  const compacted = groups.map(compactContextItemGroup);
  const returned =
    options.limit === undefined ? compacted : compacted.slice(0, options.limit);

  return {
    items: returned.map((item, index) => ({
      ...item,
      rank: index + 1,
    })),
    diagnostics: {
      rawOccurrences: options.rawOccurrences ?? options.items.length,
      uniqueOccurrences: unique.length,
      exactDuplicatesRemoved: Math.max(0, options.items.length - unique.length),
      groupsFound: compacted.length,
      groupsReturned: returned.length,
      occurrencesCollapsed: Math.max(0, unique.length - compacted.length),
      contextWindowsMerged: groups.reduce(
        (total, group) => total + mergedContextWindowCount(group.entries),
        0,
      ),
      groupTruncated: returned.length < compacted.length,
    },
  };
}

function dedupeExactOccurrences(
  items: readonly ZvecGrepContextItem[],
): IndexedContextItem[] {
  const seen = new Set<string>();
  const unique: IndexedContextItem[] = [];
  const ranked = items
    .map((item, index) => ({ item, index }))
    .sort(
      (left, right) =>
        left.item.rank - right.item.rank || left.index - right.index,
    );

  ranked.forEach(({ item }, index) => {
    const key = exactOccurrenceKey(item);
    if (seen.has(key)) {
      return;
    }

    seen.add(key);
    unique.push({ item, index });
  });

  return unique;
}

function exactOccurrenceKey(item: ZvecGrepContextItem): string {
  const matchRange = item.excerptRange ?? item.range;
  return [
    item.kind,
    normalizePath(item.file.absolutePath),
    stableRangeKey(matchRange),
  ].join(":");
}

function groupContextItems(
  entries: readonly IndexedContextItem[],
): ContextItemGroup[] {
  const containerGroups = new Map<string, ContextItemGroup>();
  const fallbackByFile = new Map<string, IndexedContextItem[]>();

  for (const entry of entries) {
    const containerKey = structuralContainerKey(entry.item);
    if (containerKey) {
      const group = containerGroups.get(containerKey);
      if (group) {
        group.entries.push(entry);
        group.firstIndex = Math.min(group.firstIndex, entry.index);
      } else {
        containerGroups.set(containerKey, {
          firstIndex: entry.index,
          entries: [entry],
        });
      }
      continue;
    }

    const fileKey = normalizePath(entry.item.file.absolutePath);
    fallbackByFile.set(fileKey, [
      ...(fallbackByFile.get(fileKey) ?? []),
      entry,
    ]);
  }

  const groups = [
    ...containerGroups.values(),
    ...[...fallbackByFile.values()].flatMap(groupFallbackFileItems),
  ];
  return groups.sort((left, right) => left.firstIndex - right.firstIndex);
}

function structuralContainerKey(item: ZvecGrepContextItem): string | null {
  if (!item.container) {
    return null;
  }
  if (matchRange(item).kind !== "text") {
    return null;
  }

  return [
    "container",
    normalizePath(item.file.absolutePath),
    item.container.entityId,
  ].join(":");
}

function groupFallbackFileItems(
  entries: readonly IndexedContextItem[],
): ContextItemGroup[] {
  const textEntries = entries
    .filter(
      (
        entry,
      ): entry is IndexedContextItem & {
        item: ZvecGrepContextItem & {
          range: Extract<Range, { kind: "text" }>;
        };
      } => entry.item.range.kind === "text",
    )
    .sort(
      (left, right) =>
        left.item.range.startLine - right.item.range.startLine ||
        left.item.range.endLine - right.item.range.endLine ||
        left.index - right.index,
    );
  const otherEntries = entries.filter(
    (entry) => entry.item.range.kind !== "text",
  );
  const groups: ContextItemGroup[] = [];
  let current: ContextItemGroup | undefined;
  let currentEndLine = 0;

  for (const entry of textEntries) {
    if (!current || entry.item.range.startLine > currentEndLine + 1) {
      current = {
        firstIndex: entry.index,
        entries: [entry],
      };
      currentEndLine = entry.item.range.endLine;
      groups.push(current);
      continue;
    }

    current.entries.push(entry);
    current.firstIndex = Math.min(current.firstIndex, entry.index);
    currentEndLine = Math.max(currentEndLine, entry.item.range.endLine);
  }

  groups.push(
    ...otherEntries.map((entry) => ({
      firstIndex: entry.index,
      entries: [entry],
    })),
  );
  return groups;
}

function compactContextItemGroup(group: ContextItemGroup): ZvecGrepContextItem {
  const ordered = [...group.entries].sort(
    (left, right) =>
      compareRanges(matchRange(left.item), matchRange(right.item)) ||
      left.index - right.index,
  );
  const base = ordered[0]!.item;
  if (ordered.length === 1) {
    return base;
  }

  return {
    ...base,
    range:
      base.container === undefined
        ? (mergedDisplayRange(ordered.map(({ item }) => item)) ?? base.range)
        : base.range,
    container:
      mergedContainer(ordered.map(({ item }) => item)) ?? base.container,
    occurrenceCount: ordered.length,
    occurrences: sampleGroupEntries(
      ordered,
      MAX_RG_GROUP_OCCURRENCE_SAMPLES,
    ).map(({ item }) => lexicalOccurrence(item)),
  };
}

function sampleGroupEntries(
  entries: readonly IndexedContextItem[],
  limit: number,
): IndexedContextItem[] {
  if (entries.length <= limit) {
    return [...entries];
  }

  const sampled: IndexedContextItem[] = [];
  const lastIndex = entries.length - 1;
  for (let index = 0; index < limit; index++) {
    const sourceIndex = Math.round((index * lastIndex) / (limit - 1));
    sampled.push(entries[sourceIndex]!);
  }
  return sampled;
}

function mergedDisplayRange(
  items: readonly ZvecGrepContextItem[],
): Extract<Range, { kind: "text" }> | null {
  const ranges = items.map((item) => item.range);
  if (ranges.length === 0 || ranges.some((range) => range.kind !== "text")) {
    return null;
  }

  const textRanges = ranges as Extract<Range, { kind: "text" }>[];
  return mergeTextRanges(textRanges);
}

function mergedContainer(
  items: readonly ZvecGrepContextItem[],
): ZvecGrepContextItem["container"] {
  const first = items[0]?.container;
  if (!first || first.range.kind !== "text") {
    return first;
  }

  return {
    ...first,
    range: mergeTextRanges(
      items.flatMap((item) =>
        item.container?.range.kind === "text" ? [item.container.range] : [],
      ),
    ),
  };
}

function mergeTextRanges(
  ranges: readonly Extract<Range, { kind: "text" }>[],
): Extract<Range, { kind: "text" }> {
  let startLine = ranges[0]!.startLine;
  let startOffset = ranges[0]!.startOffset;
  let endLine = ranges[0]!.endLine;
  let endOffset = ranges[0]!.endOffset;

  for (const range of ranges.slice(1)) {
    if (
      range.startLine < startLine ||
      (range.startLine === startLine && range.startOffset < startOffset)
    ) {
      startLine = range.startLine;
      startOffset = range.startOffset;
    }
    if (
      range.endLine > endLine ||
      (range.endLine === endLine && range.endOffset > endOffset)
    ) {
      endLine = range.endLine;
      endOffset = range.endOffset;
    }
  }

  return {
    kind: "text",
    startLine,
    endLine,
    startOffset,
    endOffset,
  };
}

function lexicalOccurrence(
  item: ZvecGrepContextItem,
): ZvecGrepLexicalOccurrence {
  return {
    rank: item.rank,
    range: item.range,
    excerptRange: item.excerptRange,
    content: item.content,
  };
}

function matchRange(item: ZvecGrepContextItem): Range {
  return item.excerptRange ?? item.range;
}

function compareRanges(left: Range, right: Range): number {
  if (left.kind === "text" && right.kind === "text") {
    return (
      left.startLine - right.startLine ||
      left.startOffset - right.startOffset ||
      left.endLine - right.endLine ||
      left.endOffset - right.endOffset
    );
  }

  return stableRangeKey(left).localeCompare(stableRangeKey(right));
}

function stableRangeKey(range: Range): string {
  switch (range.kind) {
    case "file":
      return "file";
    case "text":
      return [
        "text",
        range.startLine,
        range.endLine,
        range.startOffset,
        range.endOffset,
      ].join(":");
    case "byte":
      return ["byte", range.startOffset, range.endOffset].join(":");
    case "page":
      return ["page", range.page].join(":");
    case "page_text":
      return ["page_text", range.page, range.startOffset, range.endOffset].join(
        ":",
      );
    case "page_region":
      return [
        "page_region",
        range.page,
        range.x,
        range.y,
        range.width,
        range.height,
      ].join(":");
  }
}

function mergedContextWindowCount(
  entries: readonly IndexedContextItem[],
): number {
  const ranges = entries
    .map(({ item }) => item.range)
    .filter(
      (range): range is Extract<Range, { kind: "text" }> =>
        range.kind === "text",
    )
    .sort(
      (left, right) =>
        left.startLine - right.startLine || left.endLine - right.endLine,
    );
  if (ranges.length < 2) {
    return 0;
  }

  let merged = 0;
  let endLine = ranges[0]!.endLine;
  for (const range of ranges.slice(1)) {
    if (range.startLine <= endLine + 1) {
      merged++;
      endLine = Math.max(endLine, range.endLine);
    } else {
      endLine = range.endLine;
    }
  }
  return merged;
}
