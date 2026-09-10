import { realpath } from "node:fs/promises";
import type {
  ZvecGrepContextFile,
  ZvecGrepContextItem,
  ZvecGrepContextResult,
} from "../engine/service/types.js";
import type { Range } from "../engine/types.js";

export type LiveIndexMerge = {
  result: ZvecGrepContextResult;
  staleItemsOmitted: number;
};

/** Resolve only recalled file paths, at most once each for this CLI request. */
export async function liveIndexPathIdentities(
  indexed: ZvecGrepContextResult,
  live: ZvecGrepContextResult | undefined,
  resolvePath: (path: string) => Promise<string> = realpath,
): Promise<ReadonlyMap<string, string>> {
  if (!canMerge(indexed, live)) return new Map();
  const paths = new Set(
    [...live.items, ...(indexed.groupResults?.[0]?.items ?? indexed.items)]
      .filter((item) => item.status === "fresh")
      .map((item) => item.file.absolutePath),
  );
  return new Map(
    await Promise.all(
      [...paths].map(
        async (path) =>
          [path, await resolvePath(path).catch(() => path)] as const,
      ),
    ),
  );
}

/**
 * One ordinary CLI query has one result list, regardless of index readiness.
 * Live literal evidence precedes approximate index hits; known stale source is
 * not shown as an answer. Explicit routes and MCP grouping are not changed.
 */
export function mergeLiveAndIndexedResults(
  indexed: ZvecGrepContextResult,
  live: ZvecGrepContextResult | undefined,
  limit = 10,
  pathIdentities?: ReadonlyMap<string, string>,
): LiveIndexMerge {
  if (!canMerge(indexed, live)) {
    return { result: indexed, staleItemsOmitted: 0 };
  }
  const recalled = indexed.groupResults?.[0]?.items ?? indexed.items;
  const current = live.items.filter((item) => item.status === "fresh");
  const freshIndex = recalled.filter((item) => item.status === "fresh");
  const staleItemsOmitted = recalled.length - freshIndex.length;
  const unique: ZvecGrepContextItem[] = [];
  const displayFiles = new Map<string, ZvecGrepContextFile>();
  const withDisplayFile = (item: ZvecGrepContextItem): ZvecGrepContextItem => {
    const identity =
      pathIdentities?.get(item.file.absolutePath) ?? item.file.absolutePath;
    const displayFile = displayFiles.get(identity);
    if (!displayFile) {
      // Prefer the live scan's existing display path, never a canonical path
      // such as /private/var/... in a user-facing relative path. Reuse this
      // identity for other symbols in the same physical file so they group.
      displayFiles.set(identity, item.file);
      return item;
    }
    return { ...item, file: displayFile };
  };
  for (const item of [...current].sort((a, b) => a.rank - b.rank)) {
    const normalized = withDisplayFile(item);
    if (!unique.some((previous) => sameLiveTarget(previous, normalized))) {
      unique.push(normalized);
    }
  }
  for (const item of [...freshIndex].sort((a, b) => a.rank - b.rank)) {
    const normalized = withDisplayFile(item);
    if (
      !unique.some((previous) => sameEvidence(previous, normalized, live.query))
    ) {
      unique.push(normalized);
    }
  }
  const items = unique.slice(0, limit).map((item, index) => ({
    ...item,
    rank: index + 1,
    // These describe the index-only presentation, not the merged CLI list.
    queryGroups: undefined,
    selectionReason: undefined,
    coverageGroup: undefined,
  }));
  return {
    staleItemsOmitted,
    result: {
      ...indexed,
      items,
      groupResults: undefined,
      coverage: "ranked_sample",
      diagnostics: {
        ...indexed.diagnostics,
        emptyReason: items.length
          ? undefined
          : indexed.diagnostics.semantic
            ? "semantic_incomplete"
            : "no_matches",
        index: indexed.diagnostics.index
          ? {
              ...indexed.diagnostics.index,
              hitsReturned: items.filter(
                (item) => item.kind === "indexed_entity",
              ).length,
            }
          : undefined,
        rg: live.diagnostics.rg,
        keywords: live.diagnostics.keywords,
        structure: live.diagnostics.structure,
      },
    },
  };
}

function canMerge(
  indexed: ZvecGrepContextResult,
  live: ZvecGrepContextResult | undefined,
): live is ZvecGrepContextResult {
  return (
    live !== undefined &&
    live.source === "rg" &&
    indexed.source === "index" &&
    live.query === indexed.query &&
    (indexed.groupResults?.length ?? 0) <= 1
  );
}

function sameLiveTarget(
  left: ZvecGrepContextItem,
  right: ZvecGrepContextItem,
): boolean {
  return (
    left.file.absolutePath === right.file.absolutePath &&
    (left.container && right.container
      ? left.container.entityId === right.container.entityId ||
        (sameLineRange(left.container.range, right.container.range) &&
          sameSymbol(left.container.metadata, right.container.metadata))
      : JSON.stringify(left.excerptRange ?? left.range) ===
        JSON.stringify(right.excerptRange ?? right.range))
  );
}

function sameEvidence(
  live: ZvecGrepContextItem,
  indexed: ZvecGrepContextItem,
  query: string,
): boolean {
  if (live.file.absolutePath !== indexed.file.absolutePath) return false;
  if (live.kind !== "lexical_match") {
    return live.entityId !== undefined && live.entityId === indexed.entityId;
  }
  if (live.container) {
    // Live extraction IDs have their own namespace. Match the actual symbol
    // range, not an overlapping parent class or an unrelated same-line symbol.
    return (
      sameLineRange(live.container.range, indexed.range) &&
      sameSymbol(live.container.metadata, indexed.metadata)
    );
  }
  const match = live.excerptRange ?? live.range;
  const source = indexed.excerptRange ?? indexed.range;
  if (
    indexed.contentRole === "outline" ||
    match.kind !== "text" ||
    source.kind !== "text" ||
    source.startLine > match.startLine ||
    source.endLine < match.endLine
  ) {
    return false;
  }
  const ignoreCase = query === query.toLowerCase();
  return ignoreCase
    ? indexed.content.toLowerCase().includes(query)
    : indexed.content.includes(query);
}

function sameLineRange(left: Range, right: Range): boolean {
  return (
    left.kind === "text" &&
    right.kind === "text" &&
    left.startLine === right.startLine &&
    left.endLine === right.endLine
  );
}

function sameSymbol(
  left: ZvecGrepContextItem["metadata"],
  right: ZvecGrepContextItem["metadata"],
): boolean {
  if (left?.kind === "code" && right?.kind === "code") {
    return left.symbolName === right.symbolName && left.scope === right.scope;
  }
  if (left?.kind === "markdown" && right?.kind === "markdown") {
    return left.heading === right.heading && left.scope === right.scope;
  }
  return false;
}
