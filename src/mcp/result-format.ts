import type { ZvecGrepContextResult } from "../index.js";
import { formatAgentContextResult } from "../cli/format/context.js";

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
  return [
    `query: ${result.query}`,
    `root: ${result.root}`,
    `source: ${result.source}`,
    `coverage: ${result.coverage}`,
    `hits: ${result.items.length}`,
    "",
    formatAgentContextResult(result, {}),
  ].join("\n");
}

export function simplifyContextResult(
  result: ZvecGrepContextResult,
): Record<string, unknown> {
  return {
    query: result.query,
    root: result.root,
    source: result.source,
    coverage: result.coverage,
    collection: result.collection,
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
