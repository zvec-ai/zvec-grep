import { readFile, stat } from "node:fs/promises";
import { relative } from "node:path";
import { extract } from "../extraction/index.js";
import type { EntityFragment, FileInfo, Range } from "../types.js";
import type {
  ZvecGrepStructureEnrichmentDiagnostics,
  ZvecGrepContextContainer,
  ZvecGrepContextItem,
} from "./types.js";
import { detectFileType } from "../files/file-type.js";
import { normalizePath, toDisplayPath } from "../utils/path.js";
import { sha256Text } from "../utils/hash.js";

export const RG_STRUCTURE_ENRICH_FILE_LIMIT = 100;

type StructureEnrichmentResult = {
  items: ZvecGrepContextItem[];
  diagnostics: ZvecGrepStructureEnrichmentDiagnostics;
};

const STRUCTURE_ENRICH_COLLECTION_ID = "__rg_structure__";
const STRUCTURE_ENRICH_MAX_FILE_SIZE_BYTES = 1 * 1024 * 1024;

export async function enrichLexicalItemsWithStructure(
  root: string,
  items: readonly ZvecGrepContextItem[],
  fileLimit = RG_STRUCTURE_ENRICH_FILE_LIMIT,
): Promise<StructureEnrichmentResult> {
  const matchedFiles = uniqueLexicalFilePaths(items);
  const selectedFiles = new Set(matchedFiles.slice(0, fileLimit));
  const fragmentsByFile = new Map<string, EntityFragment[] | null>();
  let parsedFiles = 0;

  for (const absolutePath of selectedFiles) {
    const fragments = await parseStructuralFragments(root, absolutePath);
    fragmentsByFile.set(absolutePath, fragments);
    if (fragments !== null) {
      parsedFiles++;
    }
  }

  let enrichedItems = 0;
  const enrichedFiles = new Set<string>();
  const enriched = items.map((item) => {
    if (item.kind !== "lexical_match") {
      return item;
    }

    const fragments = fragmentsByFile.get(
      normalizePath(item.file.absolutePath),
    );
    if (!fragments || fragments.length === 0) {
      return item;
    }

    const matchRange = lexicalMatchRange(item);
    if (!matchRange) {
      return item;
    }

    const container = smallestContainingFragment(fragments, matchRange);
    if (!container) {
      return item;
    }

    enrichedItems++;
    enrichedFiles.add(item.file.absolutePath);

    const contextContainer: ZvecGrepContextContainer = {
      entityId: container.group ?? container.id,
      range: container.range,
      metadata: container.metadata,
    };

    return {
      ...item,
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
): Promise<EntityFragment[] | null> {
  try {
    const file = await fileInfoForStructure(root, absolutePath);
    if (!file) {
      return null;
    }

    const text = await readFile(absolutePath, "utf8");
    const source = {
      kind: "text",
      file,
      text,
    } as const;
    const fragments = await extract(source);
    const structuralFragments = fragments.filter(isStructuralFragment);
    return structuralFragments.length > 0 ? structuralFragments : null;
  } catch {
    return null;
  }
}

function isStructuralFragment(fragment: EntityFragment): boolean {
  return (
    fragment.metadata?.kind === "code" || fragment.metadata?.kind === "markdown"
  );
}

async function fileInfoForStructure(
  root: string,
  absolutePath: string,
): Promise<FileInfo | null> {
  const info = await stat(absolutePath).catch(() => null);
  if (
    !info ||
    !info.isFile() ||
    info.size <= 0 ||
    info.size > STRUCTURE_ENRICH_MAX_FILE_SIZE_BYTES
  ) {
    return null;
  }

  const detected = detectFileType(absolutePath);
  if (!detected || !isStructurallyEnrichableFile(detected)) {
    return null;
  }

  return {
    id: makeStructureFileId(absolutePath),
    collectionId: STRUCTURE_ENRICH_COLLECTION_ID,
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
    `${STRUCTURE_ENRICH_COLLECTION_ID}\0${normalizePath(absolutePath)}`,
  );
}
