import type { CliOptions } from "../types.js";

export function shouldUseColor(options: CliOptions): boolean {
  const mode = options.color ?? "auto";
  if (mode === "always") {
    return true;
  }

  if (mode === "never") {
    return false;
  }

  return process.env.NO_COLOR === undefined && process.stdout.isTTY === true;
}

export function createHighlighter(
  query: string,
  enabled: boolean,
): (value: string) => string {
  if (!enabled) {
    return (value) => value;
  }

  const terms = highlightTerms(query);
  if (terms.length === 0) {
    return (value) => value;
  }

  return (value) => highlightTermsInText(value, terms);
}

function highlightTerms(query: string): string[] {
  const ignored = new Set([
    "a",
    "an",
    "and",
    "are",
    "as",
    "by",
    "class",
    "enum",
    "find",
    "for",
    "from",
    "function",
    "how",
    "in",
    "interface",
    "is",
    "method",
    "of",
    "on",
    "or",
    "struct",
    "the",
    "to",
    "type",
    "what",
    "where",
  ]);
  const terms = new Set<string>();
  const trimmed = query.trim();

  if (trimmed.length > 1) {
    terms.add(trimmed);
  }

  for (const match of trimmed.matchAll(
    /[A-Za-z_~][A-Za-z0-9_~]*|[0-9]+(?:\.[0-9]+)?/g,
  )) {
    const term = match[0];
    if (term.length >= 2 && !ignored.has(term.toLowerCase())) {
      terms.add(term);
    }
  }

  return [...terms].sort((left, right) => right.length - left.length);
}

function highlightTermsInText(value: string, terms: readonly string[]): string {
  const ranges: Array<{ start: number; end: number }> = [];
  const lower = value.toLowerCase();

  for (const term of terms) {
    const lowerTerm = term.toLowerCase();
    let offset = 0;

    while (lowerTerm.length > 0) {
      const start = lower.indexOf(lowerTerm, offset);
      if (start < 0) {
        break;
      }

      ranges.push({ start, end: start + lowerTerm.length });
      offset = start + Math.max(lowerTerm.length, 1);
    }
  }

  if (ranges.length === 0) {
    return value;
  }

  const merged = mergeRanges(ranges);
  let highlighted = "";
  let cursor = 0;

  for (const range of merged) {
    highlighted += value.slice(cursor, range.start);
    highlighted += `\x1b[1;33m${value.slice(range.start, range.end)}\x1b[0m`;
    cursor = range.end;
  }

  return highlighted + value.slice(cursor);
}

function mergeRanges(
  ranges: Array<{ start: number; end: number }>,
): Array<{ start: number; end: number }> {
  const sorted = ranges
    .filter((range) => range.end > range.start)
    .sort((left, right) => left.start - right.start || right.end - left.end);
  const merged: Array<{ start: number; end: number }> = [];

  for (const range of sorted) {
    const previous = merged.at(-1);
    if (!previous || range.start > previous.end) {
      merged.push({ ...range });
      continue;
    }

    previous.end = Math.max(previous.end, range.end);
  }

  return merged;
}
