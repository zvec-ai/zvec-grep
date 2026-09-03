import { parseDocument } from "yaml";
import type {
  MarkdownEntityMetadata,
  MarkdownFrontMatterValue,
} from "../../types.js";

const MAX_FRONT_MATTER_FIELDS = 64;
const MAX_FRONT_MATTER_KEY_CHARS = 128;
const MAX_FRONT_MATTER_SCALAR_CHARS = 2_048;
const MAX_FRONT_MATTER_ARRAY_ITEMS = 64;
const MAX_FRONT_MATTER_TOTAL_CHARS = 16_384;

export type MarkdownDocumentMetadata = Pick<
  MarkdownEntityMetadata,
  "frontMatter"
>;

export type MarkdownFrontMatter = {
  bodyStartIndex: number;
  metadata: MarkdownDocumentMetadata;
};

export function parseMarkdownFrontMatter(
  lines: readonly string[],
): MarkdownFrontMatter | undefined {
  if (lines.length === 0 || !isOpeningDelimiter(lines[0])) {
    return undefined;
  }

  const closingIndex = lines.findIndex(
    (line, index) => index > 0 && isClosingDelimiter(line),
  );
  if (closingIndex < 0) {
    return undefined;
  }

  return {
    bodyStartIndex: closingIndex + 1,
    metadata: parseDocumentMetadata(lines.slice(1, closingIndex).join("\n")),
  };
}

function isOpeningDelimiter(line: string): boolean {
  return /^---[\t ]*\r?$/.test(line.replace(/^\uFEFF/, ""));
}

function isClosingDelimiter(line: string): boolean {
  return /^(?:---|\.\.\.)[\t ]*\r?$/.test(line);
}

function parseDocumentMetadata(source: string): MarkdownDocumentMetadata {
  let value: unknown;
  try {
    const document = parseDocument(source, {
      schema: "failsafe",
      stringKeys: true,
      prettyErrors: false,
      logLevel: "silent",
    });
    if (document.errors.length > 0) {
      return {};
    }
    value = document.toJS({ maxAliasCount: 100 });
  } catch {
    return {};
  }

  if (!isRecord(value)) {
    return {};
  }

  const fields = new Map<string, MarkdownFrontMatterValue>();
  for (const [key, fieldValue] of Object.entries(value)) {
    const normalizedKey = normalizeKey(key);
    const normalizedValue = normalizeValue(fieldValue);
    if (
      normalizedKey.length === 0 ||
      normalizedKey.length > MAX_FRONT_MATTER_KEY_CHARS ||
      normalizedValue === undefined
    ) {
      continue;
    }

    if (fields.size >= MAX_FRONT_MATTER_FIELDS && !fields.has(normalizedKey)) {
      continue;
    }
    fields.set(normalizedKey, normalizedValue);
  }

  const entries: [string, MarkdownFrontMatterValue][] = [];
  let usedChars = 0;
  for (const [key, fieldValue] of fields) {
    const fieldChars = key.length + valueChars(fieldValue);
    if (usedChars + fieldChars > MAX_FRONT_MATTER_TOTAL_CHARS) {
      continue;
    }
    entries.push([key, fieldValue]);
    usedChars += fieldChars;
  }

  return entries.length > 0 ? { frontMatter: Object.fromEntries(entries) } : {};
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function normalizeKey(value: string): string {
  return value
    .trim()
    .replace(/([A-Z]+)([A-Z][a-z])/g, "$1_$2")
    .replace(/([a-z\d])([A-Z])/g, "$1_$2")
    .replace(/[^\p{L}\p{N}]+/gu, "_")
    .replace(/^_+|_+$/g, "")
    .toLowerCase();
}

function normalizeValue(value: unknown): MarkdownFrontMatterValue | undefined {
  if (!Array.isArray(value)) {
    return normalizeScalar(value);
  }

  const normalized = [
    ...new Set(
      value
        .map(normalizeScalar)
        .filter((item): item is string => item !== undefined),
    ),
  ].slice(0, MAX_FRONT_MATTER_ARRAY_ITEMS);
  return normalized.length > 0 ? normalized : undefined;
}

function normalizeScalar(value: unknown): string | undefined {
  if (
    typeof value !== "string" &&
    typeof value !== "number" &&
    typeof value !== "boolean" &&
    typeof value !== "bigint"
  ) {
    return undefined;
  }

  const normalized = String(value).replace(/\s+/g, " ").trim();
  if (normalized.length === 0) {
    return undefined;
  }
  return truncateScalar(normalized, MAX_FRONT_MATTER_SCALAR_CHARS);
}

function truncateScalar(value: string, maxChars: number): string {
  if (value.length <= maxChars) {
    return value;
  }
  const end =
    isHighSurrogate(value.charCodeAt(maxChars - 1)) &&
    isLowSurrogate(value.charCodeAt(maxChars))
      ? maxChars - 1
      : maxChars;
  return value.slice(0, end);
}

function valueChars(value: MarkdownFrontMatterValue): number {
  return typeof value === "string"
    ? value.length
    : value.reduce((total, item) => total + item.length + 1, 0);
}

function isHighSurrogate(value: number): boolean {
  return value >= 0xd800 && value <= 0xdbff;
}

function isLowSurrogate(value: number): boolean {
  return value >= 0xdc00 && value <= 0xdfff;
}
