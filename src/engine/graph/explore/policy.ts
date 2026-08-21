import type { StoredEntity } from "../../storage/index.js";
import type { GraphQueryStorage } from "../ports.js";
import type { GraphEdgeKind } from "../types.js";

const TYPEISH_KINDS = new Set([
  "class",
  "interface",
  "struct",
  "trait",
  "enum",
  "type",
  "typealias",
]);
const QUERY_STOP_WORDS = new Set([
  "about",
  "after",
  "before",
  "does",
  "from",
  "have",
  "how",
  "into",
  "reach",
  "that",
  "their",
  "then",
  "this",
  "through",
  "what",
  "when",
  "where",
  "which",
  "with",
  "work",
  "works",
]);

export const EXPLORE_POLICY = {
  searchLimit: 8,
  traversalDepth: 3,
  maxNodes: 200,
  maxFiles: 8,
  maxChars: 24_000,
  glueLimit: 60,
  containerGlueLimit: 40,
  pathLimit: 8,
  blastLimit: 20,
  hierarchyBudgetRatio: 0.25,
  traverseEdgeKinds: [
    "CALLS",
    "REFS",
    "INHERITS",
    "CONTAINS",
    "INSTANTIATES",
  ] as const,
  rwrEdgeWeights: {
    CALLS: 1,
    INHERITS: 0.9,
    CONTAINS: 0.7,
    REFS: 0.5,
    DEFINES: 0.4,
    IMPORTS: 0.4,
    INSTANTIATES: 0.6,
  } satisfies Readonly<Record<GraphEdgeKind, number>>,
};

export function isTestPath(path: string): boolean {
  return /(^|\/)(tests?|specs?|__tests__)(\/|$)|(?:\.|_)(?:test|spec)\.[^/]+$|_test\.[^/]+$/i.test(
    path,
  );
}

export function resolveExploreSeeds(
  storage: GraphQueryStorage,
  query: string,
  seedId: string | undefined,
  limit: number,
): string[] {
  if (seedId) {
    return storage.getEntity(seedId) ? [seedId] : [];
  }

  const candidates = new Map<
    string,
    { entity: StoredEntity; exact: boolean }
  >();
  const seen = new Set<string>();
  const pushEntity = (entity: StoredEntity, exact = false) => {
    if (seen.has(entity.entity.id)) {
      if (exact) candidates.get(entity.entity.id)!.exact = true;
      return;
    }
    seen.add(entity.entity.id);
    candidates.set(entity.entity.id, { entity, exact });
  };

  const exact = storage.findSymbolsByName(query, limit);
  for (const entity of exact) {
    pushEntity(entity, true);
  }

  if (candidates.size < limit && storage.findSymbolsByQuery) {
    for (const entity of storage.findSymbolsByQuery(query, limit * 4)) {
      pushEntity(entity);
    }
  }

  for (const term of queryTerms(query)) {
    if (term.length < 2) {
      continue;
    }
    for (const entity of storage.findSymbolsByName(term, limit)) {
      pushEntity(entity, symbolName(entity).toLowerCase() === term);
    }
    if (storage.findSymbolsByQuery && term.length >= 3) {
      for (const entity of storage.findSymbolsByQuery(term, limit)) {
        pushEntity(entity);
      }
    }
  }

  const terms = queryTerms(query);
  const asksForTests = /\b(?:test|tests|testing|spec|specs)\b/i.test(query);
  const scored = [...candidates.values()].map(({ entity, exact }) => {
    const meta = entity.entity.metadata;
    const kind = meta?.kind === "code" ? meta.symbolType : "";
    const hay =
      `${symbolName(entity)} ${entity.file.relativePath}`.toLowerCase();
    const termHits = terms.filter((term) => hay.includes(term)).length;
    const testPenalty =
      !asksForTests && isTestPath(entity.file.relativePath) ? 20 : 0;
    const score =
      (exact ? 100 : 0) +
      termHits * 12 +
      (termHits >= 2 ? 20 : 0) +
      (TYPEISH_KINDS.has(kind) ? 4 : 0) -
      testPenalty;
    return { id: entity.entity.id, score };
  });
  scored.sort((a, b) => b.score - a.score || a.id.localeCompare(b.id));
  return scored.map((s) => s.id).slice(0, limit);
}

export function isTypeishKind(kind: string): boolean {
  return TYPEISH_KINDS.has(kind);
}

export function symbolName(entity: StoredEntity): string {
  const meta = entity.entity.metadata;
  return meta?.kind === "code" ? (meta.symbolName ?? "") : "";
}

export function queryTerms(query: string): string[] {
  const raw = query.match(/[A-Za-z_][A-Za-z0-9_]{1,}/g) ?? [];
  const terms = new Set<string>();
  for (const token of raw) {
    const normalized = token.toLowerCase();
    if (normalized.length >= 3 && !QUERY_STOP_WORDS.has(normalized)) {
      terms.add(normalized);
    }
    for (const piece of token.split(/(?=[A-Z])|_+/)) {
      const normalizedPiece = piece.toLowerCase();
      if (
        normalizedPiece.length >= 3 &&
        !QUERY_STOP_WORDS.has(normalizedPiece)
      ) {
        terms.add(normalizedPiece);
      }
    }
  }
  return [...terms].slice(0, 16);
}
