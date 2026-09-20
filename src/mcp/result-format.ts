import type { ZvecGrepContextResult } from "../index.js";

export type ToolResult = {
  content: Array<{ type: "text"; text: string }>;
  structuredContent?: Record<string, unknown>;
};

export function textToolResult(text: string): ToolResult {
  return {
    content: [{ type: "text", text }],
  };
}

export function toolResult(
  text: string,
  structuredContent: Record<string, unknown>,
): ToolResult {
  return {
    content: [{ type: "text", text }],
    structuredContent,
  };
}

export function contextToolResult(result: ZvecGrepContextResult): ToolResult {
  return toolResult(contextText(result), {
    result: simplifyContextResult(result),
  });
}

export function contextText(result: ZvecGrepContextResult): string {
  const lines = [
    `query: ${result.query}`,
    `root: ${result.root}`,
    `source: ${result.source}`,
    `coverage: ${result.coverage}`,
    `hits: ${result.items.length}`,
  ];

  for (const item of result.items) {
    lines.push("");
    lines.push(
      `${item.file.relativePath}:${rangeLabel(item.range)} rank=${item.rank} matchedBy=${item.matchedBy}`,
    );
    if (item.outline) {
      lines.push(`outline: ${item.outline}`);
    }
    lines.push("source:");
    lines.push(item.content);
  }

  return lines.join("\n");
}

export function simplifyContextResult(
  result: ZvecGrepContextResult,
): Record<string, unknown> {
  return {
    query: result.query,
    root: result.root,
    source: result.source,
    coverage: result.coverage,
    workspaceIndex: result.workspaceIndex,
    diagnostics: result.diagnostics,
    items: result.items.map((item) => ({
      kind: item.kind,
      rank: item.rank,
      file: item.file,
      range: item.range,
      excerptRange: item.excerptRange,
      outline: item.outline,
      content: item.content,
      contentRole: item.contentRole,
      status: item.status,
      score: item.score,
      matchedBy: item.matchedBy,
      metadata: item.metadata,
      entityId: item.entityId,
      container: item.container,
      trace: item.trace,
    })),
  };
}

function rangeLabel(range: {
  kind: string;
  startLine?: number;
  endLine?: number;
  startOffset?: number;
  endOffset?: number;
}): string {
  if (
    range.kind === "text" &&
    typeof range.startLine === "number" &&
    typeof range.endLine === "number"
  ) {
    return `${range.startLine}-${range.endLine}`;
  }
  if (
    typeof range.startOffset === "number" &&
    typeof range.endOffset === "number"
  ) {
    return `${range.startOffset}-${range.endOffset}`;
  }
  return range.kind;
}
