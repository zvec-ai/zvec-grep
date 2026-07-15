import type { Range } from "../../engine/types.js";

export function rangeLabel(range: Range): string {
  if (range.kind === "text") {
    return range.startLine === range.endLine
      ? String(range.startLine)
      : `${range.startLine}-${range.endLine}`;
  }

  if (range.kind === "byte") {
    return `bytes:${range.startOffset}-${range.endOffset}`;
  }

  if (range.kind === "page") {
    return `page:${range.page}`;
  }

  if (range.kind === "page_text") {
    return `page:${range.page}`;
  }

  if (range.kind === "page_region") {
    return `page:${range.page}`;
  }

  return "file";
}

export function rangeStartLine(range: Range): number {
  return range.kind === "text" ? range.startLine : 0;
}
