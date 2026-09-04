import type { Content, EntityFragment, EntityMetadata } from "../types.js";
import type { ChunkOptions } from "./types.js";

const MAX_METADATA_BUDGET_RATIO = 0.25;

export function vectorContentForFragment(
  fragment: EntityFragment,
  embeddingContent: Content = fragment.content,
  maxChars?: number,
): Content {
  if (embeddingContent.kind !== "text") {
    return embeddingContent;
  }

  const metadata = vectorMetadataText(
    fragment.metadata,
    metadataBudget(maxChars),
  );
  if (metadata.length === 0) {
    return embeddingContent;
  }

  return {
    kind: "text",
    text: `${metadata}\n${embeddingContent.text}`,
  };
}

export function chunkOptionsForMetadata(
  options: Required<ChunkOptions>,
  metadata: EntityMetadata | undefined,
): Required<ChunkOptions> {
  const metadataText = vectorMetadataText(
    metadata,
    metadataBudget(options.maxChunkChars),
  );
  const separatorChars = metadataText.length > 0 ? 1 : 0;
  const maxChunkChars = Math.max(
    1,
    options.maxChunkChars - metadataText.length - separatorChars,
  );
  const chunkOverlapChars = Math.min(
    options.chunkOverlapChars,
    Math.max(0, maxChunkChars - 1),
  );

  return { maxChunkChars, chunkOverlapChars };
}

export function fitTextToChars(value: string, maxChars: number): string {
  if (value.length <= maxChars) {
    return value;
  }
  if (maxChars <= 3) {
    return ".".repeat(Math.max(0, maxChars));
  }
  const rawEnd = maxChars - 3;
  const end =
    rawEnd > 0 &&
    rawEnd < value.length &&
    isHighSurrogate(value.charCodeAt(rawEnd - 1)) &&
    isLowSurrogate(value.charCodeAt(rawEnd))
      ? rawEnd - 1
      : rawEnd;
  return `${value.slice(0, end).trimEnd()}...`;
}

function vectorMetadataText(
  metadata: EntityMetadata | undefined,
  maxChars?: number,
): string {
  if (!metadata) {
    return "";
  }

  if (metadata.kind === "code") {
    return compactMetadataLines(
      [
        metadata.symbolName
          ? `symbol: ${metadata.symbolType} ${metadata.symbolName}`
          : `symbol: ${metadata.symbolType}`,
        metadata.scope ? `scope: ${metadata.scope}` : null,
        metadata.signature ? `signature: ${oneLine(metadata.signature)}` : null,
        metadata.modifiers.length > 0
          ? `modifiers: ${metadata.modifiers.join(" ")}`
          : null,
        metadata.doc ? `doc: ${oneLine(metadata.doc)}` : null,
      ],
      maxChars,
    );
  }

  return compactMetadataLines(
    [
      metadata.heading ? `heading: ${metadata.heading}` : null,
      typeof metadata.level === "number"
        ? `heading_level: ${metadata.level}`
        : null,
      metadata.scope ? `scope: ${metadata.scope}` : null,
      ...markdownFrontMatterLines(metadata.frontMatter),
    ],
    maxChars,
  );
}

function markdownFrontMatterLines(
  frontMatter: Extract<EntityMetadata, { kind: "markdown" }>["frontMatter"],
): string[] {
  if (!frontMatter) {
    return [];
  }

  return Object.entries(frontMatter)
    .map(([key, value], index) => ({
      index,
      priority: frontMatterPriority(key),
      line: `${key}: ${oneLine(
        typeof value === "string" ? value : value.join(", "),
      )}`,
    }))
    .sort((left, right) =>
      left.priority === right.priority
        ? left.index - right.index
        : left.priority - right.priority,
    )
    .map(({ line }) => line);
}

function frontMatterPriority(key: string): number {
  if (key === "title") {
    return 0;
  }
  if (key === "description" || key === "summary" || key === "abstract") {
    return 1;
  }
  if (/(?:^|_)(?:name|names)$/.test(key)) {
    return 2;
  }
  if (
    /^(?:keywords?|tags?|topics?|categories|category|aliases?|authors?|languages?)$/.test(
      key,
    )
  ) {
    return 3;
  }
  return 4;
}

function metadataBudget(maxChars: number | undefined): number | undefined {
  return maxChars === undefined
    ? undefined
    : Math.max(0, Math.floor(maxChars * MAX_METADATA_BUDGET_RATIO));
}

function compactMetadataLines(
  lines: readonly (string | null)[],
  maxChars?: number,
): string {
  const text = lines
    .filter((line): line is string => line !== null && line.trim().length > 0)
    .join("\n");
  return maxChars === undefined ? text : fitTextToChars(text, maxChars);
}

function oneLine(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function isHighSurrogate(value: number): boolean {
  return value >= 0xd800 && value <= 0xdbff;
}

function isLowSurrogate(value: number): boolean {
  return value >= 0xdc00 && value <= 0xdfff;
}
