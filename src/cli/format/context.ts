import type {
  EntityMetadata,
  ZvecGrepContextFile,
  ZvecGrepContextItem,
  ZvecGrepContextResult,
} from "../../index.js";
import type { Range } from "../../engine/types.js";
import type { CliOptions, PreviewMode } from "../types.js";
import { createHighlighter, shouldUseColor } from "./highlight.js";
import { rangeLabel, rangeStartLine } from "./range.js";

type ContextItemGroup = {
  file: ZvecGrepContextFile;
  firstRank: number;
  items: ZvecGrepContextItem[];
};

const SHORT_SOURCE_MAX_LINES = 10;
const SHORT_SOURCE_CONTEXT_BEFORE = 2;
const SHORT_OUTLINE_MAX_LINES = 7;
const AGENT_PREVIEW_MAX_LINE_LENGTH = 160;
const HUMAN_PREVIEW_MAX_LINE_LENGTH = 120;

export function printAgentContextResult(
  result: ZvecGrepContextResult,
  options: CliOptions,
): void {
  const highlighter = plainText;
  const groups = groupContextItems(result.items);
  const preview = previewMode(result, options);

  if (groups.length === 0) {
    console.log(emptyContextLabel(result));
    for (const line of emptyContextDetailLines(result)) {
      console.log(line);
    }
    return;
  }

  let first = true;
  for (const group of groups) {
    for (const item of group.items.sort(compareContextItems)) {
      if (!first) {
        console.log("");
      }
      first = false;

      console.log(`${item.file.relativePath}:${rangeLabel(headerRange(item))}`);
      const matched = matchedRangeLine(item);
      const outlineLines = agentOutlineLines(item, preview);
      const sourceLines = sourceLinesForPreview(item, preview, highlighter, {
        maxLineLength: AGENT_PREVIEW_MAX_LINE_LENGTH,
      });
      for (const line of agentMetadataLines(item, [
        ...outlineLines,
        ...sourceLines,
      ])) {
        console.log(line);
      }
      for (const line of outlineLines) {
        console.log(line);
      }
      if (matched && preview !== "none") {
        console.log(matched);
      }
      if (sourceLines.length > 0) {
        if (item.kind !== "lexical_match" && preview !== "none") {
          console.log("source:");
        }
        for (const line of sourceLines) {
          console.log(line);
        }
      }
      if (options.trace) {
        const trace = traceDetailLine(item);
        if (trace) {
          console.log(`trace: ${trace}`);
        }
      }
    }
  }
}

function plainText(value: string): string {
  return value;
}

export function printHumanContextResult(
  result: ZvecGrepContextResult,
  options: CliOptions,
): void {
  const highlighter = createHighlighter(result.query, shouldUseColor(options));
  const theme = createHumanTheme(options);
  const groups = groupContextItems(result.items);
  const preview = previewMode(result, options);

  printHumanField(theme, "Context", contextLabel(result));
  printHumanField(theme, "Query", highlighter(result.query));
  const routes = routeLabel(result);
  if (routes) {
    printHumanField(theme, "Routes", theme.accent(routes));
  }
  printHumanField(theme, "Coverage", theme.status(result.coverage));
  printHumanField(theme, "Files", String(groups.length));
  printHumanField(theme, "Hits", String(result.items.length));

  if (groups.length === 0) {
    printHumanField(
      theme,
      "Reason",
      emptyContextLabel(result).replace(/\.$/, ""),
    );
    const missingPaths = result.diagnostics.fallback?.missingPaths ?? [];
    if (missingPaths.length > 0) {
      printHumanField(theme, "Missing", missingPaths.join(", "));
    }
    return;
  }

  for (const group of groups) {
    console.log("");
    printHumanField(theme, "File", theme.path(group.file.relativePath));
    printHumanField(theme, "Hits", String(group.items.length));

    for (const item of group.items.sort(compareContextItems)) {
      const score =
        typeof item.score === "number"
          ? ` score=${formatScore(item.score)}`
          : "";
      const matched = matchedRangeLine(item);

      console.log("");
      console.log(
        `  ${theme.accent(`#${item.rank}`)} ${entitySummary(item)} matchedBy=${theme.accent(item.matchedBy)}${score}`,
      );
      console.log(
        `  ${theme.label("Range")}: ${rangeLabel(item.range)}  ${theme.label("Status")}: ${theme.status(item.status)}`,
      );
      if (matched && preview !== "none") {
        console.log(
          `  ${theme.label("Matched")}: ${theme.accent(rangeLabel(item.excerptRange ?? item.range))}`,
        );
      }
      for (const line of humanMetadataLines(item.metadata)) {
        console.log(`  ${line}`);
      }
      const outlineLines = outlineLinesForPreview(item, preview);
      if (outlineLines.length > 0) {
        console.log(`  ${theme.label("Outline")}:`);
        for (const line of outlineLines) {
          console.log(`    ${line}`);
        }
      }
      const sourceLines = sourceLinesForPreview(item, preview, highlighter, {
        maxLineLength: HUMAN_PREVIEW_MAX_LINE_LENGTH,
      });
      if (sourceLines.length > 0) {
        console.log(`  ${theme.label("Source")}:`);
        for (const line of sourceLines) {
          console.log(`    ${line}`);
        }
      }
      if (options.trace) {
        const trace = traceDetailLine(item);
        if (trace) {
          console.log(`  ${theme.label("Trace")}: ${trace}`);
        }
      }
    }
  }
}

export function contextWarningLines(result: ZvecGrepContextResult): string[] {
  const missingPaths = result.diagnostics.fallback?.missingPaths ?? [];
  if (
    missingPaths.length === 0 ||
    result.diagnostics.emptyReason === "no_searchable_files"
  ) {
    return [];
  }

  return [
    `warning: skipped missing ${missingPaths.length === 1 ? "path" : "paths"}: ${missingPaths.join(", ")}`,
  ];
}

function previewMode(
  result: ZvecGrepContextResult,
  options: CliOptions,
): PreviewMode {
  if (result.source === "rg") {
    return "full";
  }

  return options.preview ?? (options.human ? "full" : "none");
}

function emptyContextLabel(result: ZvecGrepContextResult): string {
  switch (result.diagnostics.emptyReason ?? "no_matches") {
    case "no_searchable_files":
      return "No searchable files.";
    case "index_unavailable":
      return "Index unavailable.";
    case "search_failed":
      return "Search failed.";
    case "no_matches":
      return "No matches.";
  }
}

function emptyContextDetailLines(result: ZvecGrepContextResult): string[] {
  const lines: string[] = [];
  const missingPaths = result.diagnostics.fallback?.missingPaths ?? [];
  if (missingPaths.length > 0) {
    lines.push(`missing: ${missingPaths.join(", ")}`);
  }

  return lines;
}

function groupContextItems(
  items: readonly ZvecGrepContextItem[],
): ContextItemGroup[] {
  const groups = new Map<string, ContextItemGroup>();

  for (const item of items) {
    const key = item.file.relativePath;
    const existing = groups.get(key);
    if (existing) {
      existing.items.push(item);
      existing.firstRank = Math.min(existing.firstRank, item.rank);
      continue;
    }

    groups.set(key, {
      file: item.file,
      firstRank: item.rank,
      items: [item],
    });
  }

  return [...groups.values()].sort(
    (left, right) =>
      left.firstRank - right.firstRank ||
      left.file.relativePath.localeCompare(right.file.relativePath),
  );
}

function compareContextItems(
  left: ZvecGrepContextItem,
  right: ZvecGrepContextItem,
): number {
  return (
    left.rank - right.rank ||
    rangeStartLine(left.range) - rangeStartLine(right.range) ||
    rangeLabel(left.range).localeCompare(rangeLabel(right.range))
  );
}

function agentMetadataLines(
  item: ZvecGrepContextItem,
  visiblePreviewLines: readonly string[],
): string[] {
  const lines: string[] = [];

  if (item.status === "possibly_stale") {
    lines.push("status: possibly_stale");
  }

  lines.push(
    ...agentMetadataFields(item.metadata, visiblePreviewLines, {
      forceSymbol:
        item.kind === "lexical_match" && item.container !== undefined,
    }),
  );
  return lines;
}

function agentMetadataFields(
  metadata: EntityMetadata | undefined,
  visiblePreviewLines: readonly string[],
  options: { forceSymbol?: boolean } = {},
): string[] {
  if (!metadata) {
    return [];
  }

  if (metadata.kind === "code") {
    const lines: string[] = [];
    if (
      metadata.symbolName &&
      (options.forceSymbol ||
        !previewLinesContain(visiblePreviewLines, metadata.symbolName))
    ) {
      const scope = metadata.scope ? ` scope: ${metadata.scope}` : "";
      lines.push(
        `symbol: ${metadata.symbolType} ${metadata.symbolName}${scope}`,
      );
    }

    return lines;
  }

  const lines: string[] = [];
  if (metadata.heading) {
    lines.push(`heading: ${metadata.heading}`);
  }
  if (typeof metadata.level === "number") {
    lines.push(`heading_level: ${metadata.level}`);
  }
  if (metadata.scope) {
    lines.push(`scope: ${metadata.scope}`);
  }
  return lines;
}

function previewLinesContain(lines: readonly string[], value: string): boolean {
  return lines.some((line) => line.includes(value));
}

function agentOutlineLines(
  item: ZvecGrepContextItem,
  preview: PreviewMode,
): string[] {
  const lines = outlineLinesForPreview(item, preview);
  if (lines.length === 0) {
    return [];
  }

  return ["outline:", ...lines];
}

function outlineLinesForPreview(
  item: ZvecGrepContextItem,
  preview: PreviewMode,
): string[] {
  if (preview === "none") {
    return [];
  }

  const outline = outlineContent(item);
  if (!outline) {
    return [];
  }

  const lines = splitContentLines(outline);
  return preview === "short"
    ? clipLines(lines, SHORT_OUTLINE_MAX_LINES)
    : lines;
}

function outlineContent(item: ZvecGrepContextItem): string | undefined {
  if (item.contentRole === "outline") {
    return item.content;
  }

  return item.outline;
}

function itemHasSourceContent(item: ZvecGrepContextItem): boolean {
  return item.contentRole !== "outline" && item.content.length > 0;
}

function matchedRangeLine(item: ZvecGrepContextItem): string | undefined {
  if (item.kind === "lexical_match") {
    return undefined;
  }

  if (
    !item.excerptRange ||
    rangeLabel(item.excerptRange) === rangeLabel(item.range)
  ) {
    return undefined;
  }

  return `matched: ${rangeLabel(item.excerptRange)}`;
}

function humanMetadataLines(metadata: EntityMetadata | undefined): string[] {
  if (!metadata) {
    return [];
  }

  if (metadata.kind === "code") {
    const lines = [`Kind: code/${metadata.symbolType}`];
    if (metadata.symbolName) {
      lines.push(`Symbol: ${metadata.symbolName}`);
    }
    if (metadata.scope) {
      lines.push(`Scope: ${metadata.scope}`);
    }
    if (metadata.signature) {
      lines.push(
        `Signature: ${truncate(oneLine(metadata.signature), HUMAN_PREVIEW_MAX_LINE_LENGTH)}`,
      );
    }
    if (metadata.modifiers.length > 0) {
      lines.push(`Modifiers: ${metadata.modifiers.join(", ")}`);
    }
    return lines;
  }

  const lines = [`Kind: markdown`];
  if (metadata.heading) {
    lines.push(`Heading: ${metadata.heading}`);
  }
  if (typeof metadata.level === "number") {
    lines.push(`Heading level: ${metadata.level}`);
  }
  if (metadata.scope) {
    lines.push(`Scope: ${metadata.scope}`);
  }
  return lines;
}

function numberedSourceLines(
  item: ZvecGrepContextItem,
  highlighter: (value: string) => string,
): string[] {
  return sourceLineEntries(item).map((entry) =>
    formatSourceLine(
      entry,
      highlighter,
      undefined,
      sourceLineMarker(item, entry),
    ),
  );
}

type SourceLineEntry = {
  lineNumber: number | null;
  text: string;
};

function sourceLinesForPreview(
  item: ZvecGrepContextItem,
  preview: PreviewMode,
  highlighter: (value: string) => string,
  options: { maxLineLength: number },
): string[] {
  if (!itemHasSourceContent(item)) {
    return [];
  }

  if (preview === "full") {
    return numberedSourceLines(item, highlighter);
  }

  const entries = sourceLineEntries(item);
  if (entries.length === 0) {
    return [];
  }

  if (preview === "none") {
    const entry = entries[sourceAnchorIndex(item, entries)];
    return entry
      ? [
          formatSourceLine(
            entry,
            highlighter,
            options.maxLineLength,
            sourceLineMarker(item, entry),
          ),
        ]
      : [];
  }

  const lines = sourceWindowEntries(item, entries, SHORT_SOURCE_MAX_LINES);
  return lines.map((line) =>
    typeof line === "string"
      ? line
      : formatSourceLine(
          line,
          highlighter,
          options.maxLineLength,
          sourceLineMarker(item, line),
        ),
  );
}

function sourceLineEntries(item: ZvecGrepContextItem): SourceLineEntry[] {
  const lines = splitContentLines(item.content);
  const range = contentRangeForItem(item, lines.length);
  const startLine = range.kind === "text" ? range.startLine : null;

  return lines.map((line, index) => ({
    lineNumber: startLine === null ? null : startLine + index,
    text: line,
  }));
}

function contentRangeForItem(
  item: ZvecGrepContextItem,
  lineCount: number,
): Range {
  if (item.kind === "lexical_match" || !item.excerptRange) {
    return item.range;
  }

  if (item.range.kind !== "text" || item.excerptRange.kind !== "text") {
    return item.excerptRange;
  }

  const entityLineCount = textRangeLineCount(item.range);
  const excerptLineCount = textRangeLineCount(item.excerptRange);
  return lineCount <= excerptLineCount + 2 && lineCount < entityLineCount
    ? item.excerptRange
    : item.range;
}

function sourceWindowEntries(
  item: ZvecGrepContextItem,
  entries: readonly SourceLineEntry[],
  maxLines: number,
): (SourceLineEntry | string)[] {
  if (entries.length <= maxLines) {
    return [...entries];
  }

  const anchorIndex = sourceAnchorIndex(item, entries);
  const excerptLineCount =
    item.excerptRange?.kind === "text"
      ? textRangeLineCount(item.excerptRange)
      : 1;
  const before = excerptLineCount >= maxLines ? 0 : SHORT_SOURCE_CONTEXT_BEFORE;
  let start = Math.max(0, anchorIndex - before);
  if (start + maxLines > entries.length) {
    start = Math.max(0, entries.length - maxLines);
  }

  const end = Math.min(entries.length, start + maxLines);
  return [
    ...(start > 0 ? ["..."] : []),
    ...entries.slice(start, end),
    ...(end < entries.length ? ["..."] : []),
  ];
}

function sourceAnchorIndex(
  item: ZvecGrepContextItem,
  entries: readonly SourceLineEntry[],
): number {
  if (item.excerptRange?.kind !== "text") {
    return 0;
  }

  const excerptRange = item.excerptRange;
  const index = entries.findIndex(
    (entry) =>
      entry.lineNumber !== null && entry.lineNumber >= excerptRange.startLine,
  );
  return index >= 0 ? index : 0;
}

function formatSourceLine(
  entry: SourceLineEntry,
  highlighter: (value: string) => string,
  maxLineLength?: number,
  marker?: ":" | "-",
): string {
  const prefix = entry.lineNumber === null ? "" : String(entry.lineNumber);
  const text =
    maxLineLength === undefined
      ? entry.text
      : truncate(entry.text, maxLineLength);
  return marker
    ? `${prefix}${marker}\t${highlighter(text)}`
    : `${prefix}\t${highlighter(text)}`;
}

function headerRange(item: ZvecGrepContextItem): Range {
  return item.container?.range ?? item.range;
}

function sourceLineMarker(
  item: ZvecGrepContextItem,
  entry: SourceLineEntry,
): ":" | "-" | undefined {
  if (item.kind !== "lexical_match" || entry.lineNumber === null) {
    return undefined;
  }

  const matchRange = item.excerptRange ?? item.range;
  if (matchRange.kind !== "text") {
    return ":";
  }

  return entry.lineNumber >= matchRange.startLine &&
    entry.lineNumber <= matchRange.endLine
    ? ":"
    : "-";
}

function clipLines(lines: readonly string[], maxLines: number): string[] {
  return lines.length > maxLines
    ? [...lines.slice(0, maxLines), "..."]
    : [...lines];
}

function textRangeLineCount(range: Extract<Range, { kind: "text" }>): number {
  return range.endLine - range.startLine + 1;
}

function traceDetailLine(item: ZvecGrepContextItem): string | null {
  const trace = item.trace;
  if (!trace) {
    return null;
  }

  const parts = traceRecallSummaries(trace.recall);

  if (trace.fusion) {
    const forced = trace.fusion.forced ? " forced" : "";
    parts.push(`fused #${trace.fusion.rank}${forced}`);
  }

  if (trace.ranking) {
    const forced = trace.ranking.forced ? " forced" : "";
    parts.push(`reranked #${trace.ranking.rank}${forced}`);
  }

  return parts.length > 0 ? parts.join("; ") : null;
}

function traceRecallSummaries(
  trace: NonNullable<ZvecGrepContextItem["trace"]>["recall"],
): string[] {
  const byQuery = new Map<string, typeof trace>();

  for (const recall of trace) {
    const query = recall.query?.trim() || "(unknown query)";
    byQuery.set(query, [...(byQuery.get(query) ?? []), recall]);
  }

  return [...byQuery.entries()].map(([query, recalls]) => {
    const routes = recalls.map((recall) => {
      const route = traceRouteLabel(recall.routeId, recall.path);
      if (!recall.found) {
        return `${route} missing`;
      }

      const rank = recall.rank === undefined ? "#n/a" : `#${recall.rank}`;
      const forced = recall.forced ? " forced" : "";
      return `${route} ${rank}${forced}`;
    });

    return `query ${JSON.stringify(query)}: ${routes.join(", ")}`;
  });
}

function traceRouteLabel(routeId: string | undefined, path: string): string {
  return routeId && routeId !== path ? `${routeId}/${path}` : path;
}

function splitContentLines(content: string): string[] {
  const lines = content.split(/\r?\n/);
  return lines.length > 0 ? lines : [""];
}

function entitySummary(item: ZvecGrepContextItem): string {
  const metadata = item.metadata;
  if (!metadata) {
    return item.kind;
  }

  if (metadata.kind === "code") {
    return metadata.symbolName
      ? `${metadata.symbolType} ${metadata.symbolName}`
      : metadata.symbolType;
  }

  return metadata.heading ? `heading ${metadata.heading}` : "markdown";
}

function contextLabel(result: ZvecGrepContextResult): string {
  if (result.collection) {
    return result.collection.anonymous
      ? `${result.root} anonymous index`
      : `collection ${result.collection.name}`;
  }

  return `${result.root} ${result.source}`;
}

function routeLabel(result: ZvecGrepContextResult): string | undefined {
  const routes = result.diagnostics.index?.routes;
  if (!routes || routes.length === 0) {
    return undefined;
  }

  return routes.map((route) => `${route.mode}:${route.query}`).join(", ");
}

function formatScore(score: number): string {
  return Number.isInteger(score) ? String(score) : score.toFixed(4);
}

function oneLine(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function truncate(value: string, maxLength: number): string {
  if (value.length <= maxLength) {
    return value;
  }

  return `${value.slice(0, Math.max(0, maxLength - 1))}...`;
}

type HumanTheme = {
  label(value: string): string;
  accent(value: string): string;
  path(value: string): string;
  status(value: string): string;
};

function printHumanField(
  theme: HumanTheme,
  label: string,
  value: string,
): void {
  console.log(`${theme.label(label)}: ${value}`);
}

function createHumanTheme(options: CliOptions): HumanTheme {
  if (!shouldUseColor(options)) {
    return {
      label: identity,
      accent: identity,
      path: identity,
      status: identity,
    };
  }

  return {
    label: (value) => `\x1b[2m${value}\x1b[0m`,
    accent: (value) => `\x1b[1m${value}\x1b[0m`,
    path: (value) => `\x1b[36m${value}\x1b[0m`,
    status: colorHumanStatus,
  };
}

function colorHumanStatus(value: string): string {
  if (value === "fresh" || value === "lexical_exhaustive") {
    return `\x1b[32m${value}\x1b[0m`;
  }

  if (
    value === "possibly_stale" ||
    value === "lexical_truncated" ||
    value === "ranked_sample"
  ) {
    return `\x1b[33m${value}\x1b[0m`;
  }

  return `\x1b[1m${value}\x1b[0m`;
}

function identity(value: string): string {
  return value;
}
