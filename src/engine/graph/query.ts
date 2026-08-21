import type { StoredEntity } from "../storage/index.js";
import type { GraphQueryStorage } from "./ports.js";
import type { GraphReader, SymRef } from "./types.js";

export type { GraphQueryStorage } from "./ports.js";

export type GraphQueryDirection = "callers" | "callees" | "impact";

export type GraphSeedMatch = {
  id: string;
  entity: StoredEntity;
};

export type EnrichedSymRef = SymRef & {
  entity: StoredEntity | null;
};

export type GraphNeighborhoodResult = {
  available: boolean;
  direction: GraphQueryDirection;
  query: string;
  depth: number;
  limit: number;
  seeds: GraphSeedMatch[];
  /** Set when multiple seeds match and none was disambiguated. */
  ambiguous?: boolean;
  seed?: GraphSeedMatch;
  neighbors: EnrichedSymRef[];
};

export type GraphNeighborhoodOptions = {
  direction: GraphQueryDirection;
  query: string;
  depth?: number;
  limit?: number;
  /** When multiple name matches, pick this entity id. */
  seedId?: string;
};

const DEFAULT_DEPTH = 1;
const DEFAULT_LIMIT = 20;
const SEED_LOOKUP_LIMIT = 20;

export function queryGraphNeighborhood(
  graph: GraphReader,
  storage: GraphQueryStorage,
  options: GraphNeighborhoodOptions,
): GraphNeighborhoodResult {
  const depth = clampInt(options.depth ?? DEFAULT_DEPTH, 1, 10);
  const limit = clampInt(options.limit ?? DEFAULT_LIMIT, 1, 200);
  const query = options.query.trim();

  if (!graph.available) {
    return {
      available: false,
      direction: options.direction,
      query,
      depth,
      limit,
      seeds: [],
      neighbors: [],
    };
  }

  if (!query) {
    throw new Error("graph query requires a symbol name or id");
  }

  const seeds = resolveSeeds(storage, query, options.seedId);
  if (seeds.length === 0) {
    return {
      available: true,
      direction: options.direction,
      query,
      depth,
      limit,
      seeds: [],
      neighbors: [],
    };
  }

  if (seeds.length > 1) {
    return {
      available: true,
      direction: options.direction,
      query,
      depth,
      limit,
      seeds,
      ambiguous: true,
      neighbors: [],
    };
  }

  const seed = seeds[0]!;
  const refs =
    options.direction === "callers"
      ? graph.callers(seed.id, depth, limit)
      : options.direction === "impact"
        ? graph.impact(seed.id, depth, limit)
        : graph.callees(seed.id, depth, limit);

  return {
    available: true,
    direction: options.direction,
    query,
    depth,
    limit,
    seeds,
    seed,
    neighbors: enrichSymRefs(storage, refs),
  };
}

function resolveSeeds(
  storage: GraphQueryStorage,
  query: string,
  seedId: string | undefined,
): GraphSeedMatch[] {
  if (seedId) {
    const entity = storage.getEntity(seedId);
    return entity ? [{ id: seedId, entity }] : [];
  }

  const byId = storage.getEntity(query);
  if (byId) {
    return [{ id: query, entity: byId }];
  }

  const matches = storage.findSymbolsByName(query, SEED_LOOKUP_LIMIT);
  const seen = new Set<string>();
  const seeds: GraphSeedMatch[] = [];
  for (const entity of matches) {
    if (seen.has(entity.entity.id)) {
      continue;
    }
    seen.add(entity.entity.id);
    seeds.push({ id: entity.entity.id, entity });
  }
  return seeds;
}

function enrichSymRefs(
  storage: GraphQueryStorage,
  refs: readonly SymRef[],
): EnrichedSymRef[] {
  return refs.map((ref) => ({
    ...ref,
    entity: storage.getEntity(ref.id),
  }));
}

function clampInt(value: number, min: number, max: number): number {
  if (!Number.isFinite(value)) {
    return min;
  }
  return Math.min(max, Math.max(min, Math.floor(value)));
}
