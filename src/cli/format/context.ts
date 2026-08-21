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

type AgentRgBlock =
  | {
      kind: "symbol";
      firstRank: number;
      range: Range;
      label: string;
      items: ZvecGrepContextItem[];
    }
  | {
      kind: "raw";
      firstRank: number;
      items: ZvecGrepContextItem[];
    };

type AgentRgSourceLine = SourceLineEntry & {
  marker: ":" | "-" | undefined;
  order: number;
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
  for (const line of agentContextLines(result, options)) {
    console.log(line);
  }
}

/** Print the CLI query layout without the MCP cross-group fill presentation. */
export function printCliContextResult(
  result: ZvecGrepContextResult,
  options: CliOptions,
): void {
  if (options.human) {
    printCliHumanContextResult(result, options);
    return;
  }
  for (const line of cliAgentContextLines(result, options)) {
    console.log(line);
  }
}

export function formatCliContextResult(
  result: ZvecGrepContextResult,
  options: CliOptions,
): string {
  return cliAgentContextLines(result, options).join("\n");
}

export function formatAgentContextResult(
  result: ZvecGrepContextResult,
  options: CliOptions,
): string {
  return agentContextLines(result, options).join("\n");
}

function cliAgentContextLines(
  result: ZvecGrepContextResult,
  options: CliOptions,
): string[] {
  if (result.source === "rg" || result.groupResults === undefined) {
    return agentContextLines(result, options);
  }

  const lines = [`query groups (${result.groupResults.length}):`];
  for (const [index, group] of result.groupResults.entries()) {
    if (index > 0) {
      lines.push("");
    }
    lines.push(`${group.id} [${group.role}]: ${oneLine(group.query)}`);
    lines.push(`hits: ${group.items.length}`);
    const groupResult = contextResultForGroup(result, group);
    const itemLines = agentContextLines(groupResult, options);
    if (itemLines.length > 0) {
      lines.push("", ...itemLines);
    }
  }
  return lines;
}

function contextResultForGroup(
  result: ZvecGrepContextResult,
  group: NonNullable<ZvecGrepContextResult["groupResults"]>[number],
): ZvecGrepContextResult {
  return {
    ...result,
    query: group.query,
    items: group.items.map((item) => ({
      ...item,
      queryGroups: undefined,
      selectionReason: undefined,
      coverageGroup: undefined,
    })),
    groupResults: undefined,
    diagnostics: {
      ...result.diagnostics,
      emptyReason: group.items.length === 0 ? "no_matches" : undefined,
      index: result.diagnostics.index
        ? {
            ...result.diagnostics.index,
            hitsReturned: group.items.length,
            queryGroups: [
              { id: group.id, query: group.query, role: group.role },
            ],
          }
        : undefined,
    },
  };
}

function agentContextLines(
  result: ZvecGrepContextResult,
  options: CliOptions,
): string[] {
  if (result.source === "rg") {
    return agentRgContextLines(result, options);
  }

  const highlighter = shouldUseColor(options)
    ? createHighlighter(result.query, true)
    : plainText;
  const preview = previewMode(result, options);
  const items = [...result.items].sort(compareContextItems);
  const lines: string[] = [];

  if (items.length === 0) {
    return [emptyContextLabel(result), ...emptyContextDetailLines(result)];
  }

  const queryGroups = result.diagnostics.index?.queryGroups ?? [];
  if (queryGroups.length > 1) {
    lines.push(
      `query groups (${queryGroups.length}):`,
      ...queryGroups.map(
        (group) => `  ${group.id} [${group.role}]: ${group.query}`,
      ),
      "selection: primary-group coverage then global_fill; prioritized<=6; all candidates detailed",
      "",
    );
  }

  let first = true;
  for (const item of items) {
    if (!first) {
      lines.push("");
    }
    first = false;

    lines.push(agentRankedItemHeader(item, options.trace === true));
    const groupLine = agentQueryGroupLine(item);
    if (groupLine) {
      lines.push(groupLine);
    }
    const matched = matchedRangeLine(item);
    const outlineLines = agentOutlineLines(item, preview);
    const sourceLines = sourceLinesForPreview(item, preview, highlighter, {
      maxLineLength: AGENT_PREVIEW_MAX_LINE_LENGTH,
    });
    lines.push(
      ...agentMetadataLines(item, [...outlineLines, ...sourceLines]),
      ...outlineLines,
    );
    if (matched && preview !== "none") {
      lines.push(matched);
    }
    if (sourceLines.length > 0) {
      if (item.kind !== "lexical_match" && preview !== "none") {
        lines.push("source:");
      }
      lines.push(...sourceLines);
    }
    if (options.trace) {
      const trace = traceDetailLine(item);
      if (trace) {
        lines.push(`trace: ${trace}`);
      }
    }
  }

  if (result.relationships && result.relationships.length > 0) {
    lines.push("", "relationships:");
    for (const relation of result.relationships) {
      lines.push(
        `- ${relation.srcLabel} --${relation.kind}--> ${relation.dstLabel}`,
      );
    }
  }

  return lines;
}

function agentRankedItemHeader(
  item: ZvecGrepContextItem,
  trace: boolean,
): string {
  const score =
    trace && typeof item.score === "number"
      ? ` score=${formatScore(item.score)}`
      : "";
  const selection =
    item.selectionReason === "coverage"
      ? ` [group_coverage: ${item.coverageGroup ?? "unknown"}]`
      : item.selectionReason === "global_fill"
        ? " [global_fill]"
        : "";
  return `#${item.rank}${selection} matchedBy=${item.matchedBy}${score} ${item.file.relativePath}:${rangeLabel(headerRange(item))}`;
}

function agentQueryGroupLine(item: ZvecGrepContextItem): string | undefined {
  if (!item.queryGroups || item.queryGroups.length === 0) {
    return undefined;
  }
  return `groups: ${item.queryGroups
    .map((group) => `${group.id}#${group.rank} (${group.matchedBy})`)
    .join(", ")}`;
}

function agentRgContextLines(
  result: ZvecGrepContextResult,
  options: CliOptions,
): string[] {
  if (result.items.length === 0) {
    return [emptyContextLabel(result), ...emptyContextDetailLines(result)];
  }

  const highlighter = shouldUseColor(options)
    ? createHighlighter(result.query, true)
    : plainText;
  const lines: string[] = [];

  for (const group of groupContextItems(result.items)) {
    lines.push(group.file.relativePath);

    for (const block of groupAgentRgItems(group)) {
      if (block.kind === "symbol" && !isRedundantDeclarationBlock(block)) {
        lines.push(...agentRgSymbolBlockLines(block, highlighter));
      } else {
        lines.push(
          ...block.items.flatMap((item) =>
            agentRgFileSourceLines(item, highlighter),
          ),
        );
      }
    }
  }

  return lines;
}

function agentRgSymbolBlockLines(
  block: Extract<AgentRgBlock, { kind: "symbol" }>,
  highlighter: (value: string) => string,
): string[] {
  const sourceLines = agentRgSymbolSourceLines(
    block.items,
    "    ",
    highlighter,
  );
  const header = `  ${rangeLabel(block.range)} [${block.label}]`;
  if (block.items.length === 1 && sourceLines.length === 1) {
    return [`${header} ${sourceLines[0]!.trimStart()}`];
  }
  return [header, ...sourceLines];
}

function groupAgentRgItems(group: ContextItemGroup): AgentRgBlock[] {
  const blocks: AgentRgBlock[] = [];
  const symbolBlocks = new Map<
    string,
    Extract<AgentRgBlock, { kind: "symbol" }>
  >();

  for (const item of [...group.items].sort(compareContextItems)) {
    const symbol = agentRgSymbol(item);
    if (!symbol) {
      blocks.push({
        kind: "raw",
        firstRank: item.rank,
        items: [item],
      });
      continue;
    }

    const key = `${item.container!.entityId}\0${symbol.label}`;
    const existing = symbolBlocks.get(key);
    if (existing) {
      existing.firstRank = Math.min(existing.firstRank, item.rank);
      existing.range = mergeAgentRgSymbolRanges(
        existing.range,
        item.container!.range,
      );
      existing.items.push(item);
      continue;
    }

    const block: Extract<AgentRgBlock, { kind: "symbol" }> = {
      kind: "symbol",
      firstRank: item.rank,
      range: item.container!.range,
      label: symbol.label,
      items: [item],
    };
    blocks.push(block);
    symbolBlocks.set(key, block);
  }

  return blocks.sort(
    (left, right) =>
      left.firstRank - right.firstRank ||
      rangeStartLine(agentRgBlockRange(left)) -
        rangeStartLine(agentRgBlockRange(right)),
  );
}

function mergeAgentRgSymbolRanges(left: Range, right: Range): Range {
  if (left.kind !== "text" || right.kind !== "text") {
    return left;
  }

  return {
    kind: "text",
    startLine: Math.min(left.startLine, right.startLine),
    endLine: Math.max(left.endLine, right.endLine),
    startOffset:
      left.startLine <= right.startLine ? left.startOffset : right.startOffset,
    endOffset: left.endLine >= right.endLine ? left.endOffset : right.endOffset,
  };
}

function agentRgBlockRange(block: AgentRgBlock): Range {
  return block.kind === "symbol" ? block.range : block.items[0]!.range;
}

function agentRgSymbol(
  item: ZvecGrepContextItem,
): { label: string } | undefined {
  const container = item.container;
  const metadata = container?.metadata ?? item.metadata;
  if (!container || !metadata) {
    return undefined;
  }

  if (metadata.kind === "code") {
    if (!metadata.symbolName) {
      return undefined;
    }
    const qualifiedName = metadata.scope
      ? `${metadata.scope}.${metadata.symbolName}`
      : metadata.symbolName;
    return { label: `${metadata.symbolType} ${qualifiedName}` };
  }

  if (!metadata.heading) {
    return undefined;
  }
  const qualifiedHeading = metadata.scope
    ? `${metadata.scope} > ${metadata.heading}`
    : metadata.heading;
  return { label: `heading ${qualifiedHeading}` };
}

function agentRgSymbolSourceLines(
  items: readonly ZvecGrepContextItem[],
  indent: string,
  highlighter: (value: string) => string,
): string[] {
  const byLine = new Map<number, AgentRgSourceLine>();
  const unnumbered: AgentRgSourceLine[] = [];
  let order = 0;

  for (const item of [...items].sort(compareContextItems)) {
    for (const entry of sourceLineEntries(item)) {
      const candidate: AgentRgSourceLine = {
        ...entry,
        marker: sourceLineMarker(item, entry),
        order: order++,
      };
      if (entry.lineNumber === null) {
        unnumbered.push(candidate);
        continue;
      }

      const existing = byLine.get(entry.lineNumber);
      if (!existing) {
        byLine.set(entry.lineNumber, candidate);
      } else if (candidate.marker === ":" && existing.marker !== ":") {
        existing.marker = ":";
      }
    }
  }

  return [...byLine.values(), ...unnumbered]
    .sort(
      (left, right) =>
        (left.lineNumber ?? Number.MAX_SAFE_INTEGER) -
          (right.lineNumber ?? Number.MAX_SAFE_INTEGER) ||
        left.order - right.order,
    )
    .map(
      (entry) =>
        `${indent}${formatSourceLine(entry, highlighter, undefined, entry.marker)}`,
    );
}

function isRedundantDeclarationBlock(
  block: Extract<AgentRgBlock, { kind: "symbol" }>,
): boolean {
  if (block.items.length !== 1) {
    return false;
  }

  const item = block.items[0]!;
  const metadata = item.container?.metadata ?? item.metadata;
  const containerRange = item.container?.range;
  if (
    metadata?.kind !== "code" ||
    !metadata.symbolName ||
    containerRange?.kind !== "text"
  ) {
    return false;
  }

  const matchedLines = sourceLineEntries(item).filter(
    (entry) => sourceLineMarker(item, entry) === ":",
  );
  const match = matchedLines[0];
  return (
    matchedLines.length === 1 &&
    match?.lineNumber === containerRange.startLine &&
    match.text.includes(metadata.symbolName)
  );
}

function agentRgFileSourceLines(
  item: ZvecGrepContextItem,
  highlighter: (value: string) => string,
): string[] {
  return sourceLineEntries(item).map(
    (entry) =>
      `  ${formatSourceLine(
        entry,
        highlighter,
        undefined,
        sourceLineMarker(item, entry),
      )}`,
  );
}

function plainText(value: string): string {
  return value;
}

function printCliHumanContextResult(
  result: ZvecGrepContextResult,
  options: CliOptions,
): void {
  if (result.source === "rg" || result.groupResults === undefined) {
    printHumanContextResult(result, options);
    return;
  }

  const theme = createHumanTheme(options);
  const allItems = result.groupResults.flatMap((group) => group.items);
  printHumanField(theme, "Context", contextLabel(result));
  printHumanField(theme, "Query", result.query);
  const routes = routeLabel(result);
  if (routes) {
    printHumanField(theme, "Routes", theme.accent(routes));
  }
  printHumanField(theme, "Coverage", theme.status(result.coverage));
  printHumanField(theme, "Groups", String(result.groupResults.length));
  printHumanField(theme, "Files", String(groupContextItems(allItems).length));
  printHumanField(theme, "Hits", String(allItems.length));

  for (const group of result.groupResults) {
    console.log("");
    printHumanField(theme, "Group", `${group.id} [${group.role}]`);
    printHumanField(
      theme,
      "Query",
      createHighlighter(
        group.query,
        shouldUseColor(options),
      )(oneLine(group.query)),
    );
    printHumanField(theme, "Hits", String(group.items.length));
    if (group.items.length === 0) {
      printHumanField(theme, "Reason", "No matches");
      continue;
    }
    printHumanItemGroups(
      groupContextItems(group.items),
      createHighlighter(group.query, shouldUseColor(options)),
      theme,
      previewMode(result, options),
      options,
    );
  }
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
  if (result.relationships && result.relationships.length > 0) {
    printHumanField(
      theme,
      "Relationships",
      String(result.relationships.length),
    );
  }

  if (groups.length === 0) {
    printHumanField(
      theme,
      "Reason",
      emptyContextLabel(result).replace(/\.$/, ""),
    );
    const missingPaths = result.diagnostics.rg?.missingPaths ?? [];
    if (missingPaths.length > 0) {
      printHumanField(theme, "Missing", missingPaths.join(", "));
    }
    return;
  }

  printHumanItemGroups(groups, highlighter, theme, preview, options);
}

function printHumanItemGroups(
  groups: readonly ContextItemGroup[],
  highlighter: (value: string) => string,
  theme: ReturnType<typeof createHumanTheme>,
  preview: PreviewMode,
  options: CliOptions,
): void {
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
  const missingPaths = result.diagnostics.rg?.missingPaths ?? [];
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
    case "no_matches":
      return "No matches.";
  }
}

function emptyContextDetailLines(result: ZvecGrepContextResult): string[] {
  const lines: string[] = [];
  const missingPaths = result.diagnostics.rg?.missingPaths ?? [];
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
  if (result.workspaceIndex) {
    return `${result.root} workspace index`;
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
  if (value === "fresh" || value === "rg_exhaustive") {
    return `\x1b[32m${value}\x1b[0m`;
  }

  if (
    value === "possibly_stale" ||
    value === "rg_truncated" ||
    value === "ranked_sample"
  ) {
    return `\x1b[33m${value}\x1b[0m`;
  }

  return `\x1b[1m${value}\x1b[0m`;
}

function identity(value: string): string {
  return value;
}
