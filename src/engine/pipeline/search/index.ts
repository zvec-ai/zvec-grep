import {
  workspaceIndexDetail,
  detail,
  EngineError,
  errorDetails,
} from "../../errors.js";
import type { EmbeddingModel } from "../../models/index.js";
import type { WorkspaceIndexStorage } from "../../storage/index.js";
import {
  exploreSubgraph,
  type GraphEdgeKind,
  type GraphReader,
} from "../../graph/index.js";
import type {
  WorkspaceIndexInfo,
  Entity,
  EntityFragment,
  EntitySearchDiagnosis,
  FileInfo,
  ResolvedSearchPlan,
  ResolvedSearchPlanRoute,
  SearchFinalTrace,
  SearchHit,
  SearchHitEvidence,
  SearchHitTrace,
  SearchMatchedBy,
  SearchGraphRelation,
  SearchPlan,
  SearchPlanResult,
  SearchRecallTrace,
} from "../../types.js";
import { TimingCollector } from "../../utils/timing.js";
import {
  hasPathGlob,
  isAbsolutePathPattern,
  normalizePathForMatch,
  normalizePathPattern,
  pathPatternMatches,
} from "../../utils/glob.js";
import {
  matchesFileSelection,
  resolveFileTypePatterns,
  type FileTypePatterns,
} from "../../utils/file-selection.js";

type SearchContext = {
  workspaceIndex: WorkspaceIndexInfo;
  storage: WorkspaceIndexStorage;
  embeddingModel?: EmbeddingModel;
  graph?: GraphReader;
};

type StorageSearchFilter = NonNullable<
  Parameters<WorkspaceIndexStorage["searchFts"]>[2]
>;

type StorageSearchHit = ReturnType<WorkspaceIndexStorage["searchFts"]>[number];
type StoredEntity = ReturnType<
  WorkspaceIndexStorage["listEntitiesByFile"]
>[number];
type SearchSource = "fts" | "vector" | "graph";

type Candidate = {
  id: string;
  entity: Entity;
  file: FileInfo;
  sources: Set<SearchSource>;
  recall: SearchRecallTrace[];
  evidence: InternalSearchEvidence[];
  score: number;
  rank: number;
  forced: boolean;
};

type InternalSearchEvidence = {
  fragment: EntityFragment;
  path: SearchSource;
  routeId?: string;
  query?: string;
  rank?: number;
  score?: number;
  forced?: boolean;
};

type PathFilterMatcher = (file: FileInfo) => boolean;

const DEFAULT_LIMIT = 7;
const RRF_K = 60;
const RECALL_INITIAL_DEPTH = 200;
const RECALL_MAX_DEPTH = 2000;
const RECALL_GROWTH_FACTOR = 2;
const RECALL_TARGET_FACTOR = 5;
const RECALL_MIN_TARGET_CANDIDATES = 50;
const GRAPH_EXPAND_SEED_TOP_K = 5;
const GRAPH_EXPAND_DEPTH = 2;
const GRAPH_EXPAND_MAX_NODES = 80;
const GRAPH_EXPAND_RANK_PENALTY = 50;
const GRAPH_EXPAND_FILE_PER_SEED = 3;
const GRAPH_EXPAND_ENTITIES_PER_FILE = 3;
const GRAPH_EXPAND_IMPORT_RANK_PENALTY = 55;

type RecallRoute = ResolvedSearchPlanRoute & {
  filter?: StorageSearchFilter;
  vectorRouteId?: string;
};

export async function searchWorkspaceIndex(
  plan: SearchPlan,
  ctx: SearchContext,
): Promise<SearchPlanResult> {
  const timings = new TimingCollector();

  const result = await timings.time("search_total", async () => {
    const normalized = timings.timeSync("search_plan", () =>
      validateSearchPlan(plan),
    );
    const limit = normalized.limit ?? DEFAULT_LIMIT;
    const trace =
      normalized.trace === true || normalized.trackEntityId !== undefined;
    const fileTypePatterns = await timings.time("search_file_types", () =>
      resolveFileTypePatterns(
        normalized.fileTypes,
        normalized.excludedFileTypes,
      ),
    );
    const filter = timings.timeSync("search_filter", () =>
      searchPlanToStorageFilter(normalized, ctx.storage, fileTypePatterns),
    );
    const hasSearchableFiles = !filterMatchesNoFiles(filter);
    const candidates = new Map<string, Candidate>();
    const vectorByRoute =
      hasSearchableFiles && planUsesVector(normalized)
        ? await timings.time("query_embedding", () =>
            embedVectorRoutes(
              normalized.routes,
              requireEmbeddingModel(ctx, "searchPlan"),
            ),
          )
        : new Map<string, number[]>();
    let recallDepth = RECALL_INITIAL_DEPTH;

    if (hasSearchableFiles) {
      recallDepth = timings.timeSync("recall", () =>
        collectAdaptiveRecall({
          routes: normalized.routes,
          filter,
          preferSymbol: normalized.preferSymbol === true,
          vectorByRoute,
          limit,
          storage: ctx.storage,
          candidates,
        }),
      );
    }

    if (normalized.trackEntityId) {
      timings.timeSync("force_track", () =>
        forceTrackEntity({
          entityId: normalized.trackEntityId!,
          routes: normalized.routes,
          vectorByRoute,
          recallDepth,
          filter,
          storage: ctx.storage,
          candidates,
        }),
      );
    }

    let fused = timings.timeSync("fusion", () => fuseCandidates(candidates));
    const graphExpansion = timings.timeSync("graph_expand", () =>
      expandGraphNeighbors({
        graph: ctx.graph,
        storage: ctx.storage,
        candidates,
        seeds: fused,
        limit,
        filter,
      }),
    );
    if (graphExpansion.rerankNeeded) {
      fused = timings.timeSync("fusion_graph", () =>
        fuseCandidates(candidates),
      );
    }

    const visible = fused.slice(0, limit);
    const relationships = selectGraphRelationships(
      graphExpansion.relations,
      visible,
      graphExpansion.seedIds,
      candidates,
    );
    const tracked = normalized.trackEntityId
      ? fused.find((candidate) => candidate.id === normalized.trackEntityId)
      : undefined;

    if (tracked && !visible.some((candidate) => candidate.id === tracked.id)) {
      visible.push(tracked);
    }

    const hits = timings.timeSync("materialize", () =>
      visible.map((candidate) => candidateToHit(candidate, limit, trace)),
    );
    const trackedHit = normalized.trackEntityId
      ? hits.find((hit) => hit.entity.id === normalized.trackEntityId)
      : undefined;

    return {
      plan: normalized,
      hits,
      relationships,
      trackedHit,
      graphExpand: {
        available: graphExpansion.available,
        unavailableReason: graphExpansion.unavailableReason,
        seeds: graphExpansion.seeds,
        neighborsAdded: graphExpansion.neighborsAdded,
      },
    };
  });

  return {
    ...result,
    timings: timings.entries(),
  };
}

export async function diagnoseEntitySearch(
  query: string,
  entityId: string,
  ctx: SearchContext,
): Promise<EntitySearchDiagnosis> {
  const stored = ctx.storage.getEntity(entityId);

  if (!stored) {
    throw new Error(`Entity not found: ${entityId}`);
  }

  const search = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query },
        { mode: "vector", query },
      ],
      trace: true,
      trackEntityId: entityId,
    },
    ctx,
  );

  return {
    query,
    entityId,
    file: stored.file,
    entity: stored.entity,
    search,
  };
}

export async function diagnoseFileSearch(
  query: string,
  absolutePath: string,
  ctx: SearchContext,
): Promise<EntitySearchDiagnosis | null> {
  const file = ctx.storage.getFileByPath(absolutePath);
  if (!file) {
    return null;
  }

  const entityId = await chooseBestEntityInFile(query, file, ctx);
  if (!entityId) {
    return null;
  }

  return diagnoseEntitySearch(query, entityId, ctx);
}

function validateSearchPlan(plan: SearchPlan): ResolvedSearchPlan {
  if (!Array.isArray(plan.routes) || plan.routes.length === 0) {
    throw new EngineError("Search plan requires at least one route", {
      code: "ZVEC_GREP.ENGINE.SEARCH_PLAN.EMPTY_ROUTES",
    });
  }

  const routeIds = new Set<string>();
  const routeCounts = new Map<ResolvedSearchPlanRoute["mode"], number>();
  const routes = plan.routes.map((route, index): ResolvedSearchPlanRoute => {
    const query = typeof route.query === "string" ? route.query.trim() : "";

    if (route.mode !== "fts" && route.mode !== "vector") {
      throw new EngineError("Search plan route has an unsupported mode", {
        code: "ZVEC_GREP.ENGINE.SEARCH_PLAN.UNSUPPORTED_ROUTE_MODE",
        context: `routeIndex=${index} mode=${String(route.mode)}`,
      });
    }

    const id = makeDefaultRouteId(route.mode, routeCounts, routeIds);

    if (query.length === 0) {
      throw new EngineError("Search plan route requires a non-empty query", {
        code: "ZVEC_GREP.ENGINE.SEARCH_PLAN.EMPTY_ROUTE_QUERY",
        context: `routeId=${id}`,
      });
    }

    routeIds.add(id);

    return {
      id,
      mode: route.mode,
      query,
    };
  });

  const modifiedAfter = normalizeModifiedTime(
    plan.modifiedAfter,
    "modifiedAfter",
  );
  const modifiedBefore = normalizeModifiedTime(
    plan.modifiedBefore,
    "modifiedBefore",
  );

  if (
    modifiedAfter !== undefined &&
    modifiedBefore !== undefined &&
    modifiedAfter > modifiedBefore
  ) {
    throw new EngineError(
      "Search plan modified-after filter must not be later than modified-before",
      {
        code: "ZVEC_GREP.ENGINE.SEARCH_PLAN.INVALID_MODIFIED_TIME_RANGE",
        context: `modifiedAfter=${modifiedAfter} modifiedBefore=${modifiedBefore}`,
      },
    );
  }

  return {
    ...plan,
    routes,
    includePaths: normalizePathFilters(plan.includePaths, "includePaths"),
    excludePaths: normalizePathFilters(plan.excludePaths, "excludePaths"),
    globs: normalizeStringFilters(plan.globs, "globs"),
    insensitiveGlobs: normalizeStringFilters(
      plan.insensitiveGlobs,
      "insensitiveGlobs",
    ),
    fileTypes: normalizeStringFilters(plan.fileTypes, "fileTypes"),
    excludedFileTypes: normalizeStringFilters(
      plan.excludedFileTypes,
      "excludedFileTypes",
    ),
    modifiedAfter,
    modifiedBefore,
  };
}

function makeDefaultRouteId(
  mode: ResolvedSearchPlanRoute["mode"],
  routeCounts: Map<ResolvedSearchPlanRoute["mode"], number>,
  usedRouteIds: ReadonlySet<string>,
): string {
  let count = routeCounts.get(mode) ?? 0;

  while (true) {
    count++;
    const id = count === 1 ? mode : `${mode}-${count}`;

    if (!usedRouteIds.has(id)) {
      routeCounts.set(mode, count);

      return id;
    }
  }
}

function planUsesVector(plan: SearchPlan): boolean {
  return plan.routes.some((route) => route.mode === "vector");
}

function requireEmbeddingModel(
  ctx: SearchContext,
  operation: string,
): EmbeddingModel {
  if (!ctx.embeddingModel) {
    throw new EngineError("Search operation requires an embedding model", {
      code: "ZVEC_GREP.ENGINE.SEARCH.EMBEDDING_MODEL_REQUIRED",
      context: errorDetails([
        workspaceIndexDetail(ctx.workspaceIndex.name),
        detail("operation", operation),
      ]),
    });
  }

  return ctx.embeddingModel;
}

function normalizePathFilters(
  value: readonly string[] | undefined,
  field: "includePaths" | "excludePaths",
): string[] | undefined {
  if (value === undefined) {
    return undefined;
  }

  if (!Array.isArray(value)) {
    throw new EngineError("Search plan path filters must be arrays", {
      code: "ZVEC_GREP.ENGINE.SEARCH_PLAN.INVALID_PATH_FILTERS",
      context: `field=${field}`,
    });
  }

  const patterns: string[] = [];

  for (const [index, item] of value.entries()) {
    if (typeof item !== "string") {
      throw new EngineError("Search plan path filters must contain strings", {
        code: "ZVEC_GREP.ENGINE.SEARCH_PLAN.INVALID_PATH_FILTER",
        context: `field=${field} index=${index}`,
      });
    }

    const pattern = normalizePathFilterPattern(item);
    if (pattern.length > 0) {
      patterns.push(pattern);
    }
  }

  return patterns.length > 0 ? patterns : undefined;
}

function normalizeStringFilters(
  value: readonly string[] | undefined,
  field: string,
): string[] | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (!Array.isArray(value)) {
    throw new EngineError("Search plan filters must be arrays", {
      code: "ZVEC_GREP.ENGINE.SEARCH_PLAN.INVALID_FILTERS",
      context: `field=${field}`,
    });
  }
  const values = value.map((item, index) => {
    if (typeof item !== "string" || !item.trim()) {
      throw new EngineError("Search plan filters must contain strings", {
        code: "ZVEC_GREP.ENGINE.SEARCH_PLAN.INVALID_FILTER",
        context: `field=${field} index=${index}`,
      });
    }
    return item.trim();
  });
  return values.length > 0 ? values : undefined;
}

function normalizeModifiedTime(
  value: number | undefined,
  field: "modifiedAfter" | "modifiedBefore",
): number | undefined {
  if (value === undefined) {
    return undefined;
  }

  if (!Number.isFinite(value) || value < 0) {
    throw new EngineError(
      "Search plan modified time filters must be non-negative epoch milliseconds",
      {
        code: "ZVEC_GREP.ENGINE.SEARCH_PLAN.INVALID_MODIFIED_TIME_FILTER",
        context: `field=${field} value=${String(value)}`,
      },
    );
  }

  return value;
}

async function embedVectorRoutes(
  routes: readonly ResolvedSearchPlanRoute[],
  model: EmbeddingModel,
): Promise<Map<string, number[]>> {
  const vectorRoutes = routes.filter((route) => route.mode === "vector");
  const vectorsByRoute = new Map<string, number[]>();

  for (
    let start = 0;
    start < vectorRoutes.length;
    start += model.info.limits.maxBatchSize
  ) {
    const batch = vectorRoutes.slice(
      start,
      start + model.info.limits.maxBatchSize,
    );
    const { vectors } = await model.embed(
      batch.map((route) => ({
        kind: "text",
        text: route.query,
      })),
      { purpose: "query" },
    );

    for (const [index, route] of batch.entries()) {
      vectorsByRoute.set(route.id, vectors[index]);
    }
  }

  return vectorsByRoute;
}

function addRecallHits(
  candidates: Map<string, Candidate>,
  hits: readonly StorageSearchHit[],
  route: ResolvedSearchPlanRoute,
  storage: WorkspaceIndexStorage,
  options: { startIndex?: number } = {},
): void {
  const startIndex = Math.max(0, options.startIndex ?? 0);

  for (let index = startIndex; index < hits.length; index++) {
    const hit = hits[index];
    if (!hit) {
      continue;
    }

    const rank = index + 1;
    const entityId = publicEntityId(hit.fragment);
    const resolved = candidates.get(entityId) ?? resolveHitEntity(hit, storage);

    if (!resolved) {
      continue;
    }

    const candidate = candidates.get(entityId) ?? {
      id: entityId,
      entity: resolved.entity,
      file: resolved.file,
      sources: new Set<"fts" | "vector">(),
      recall: [],
      evidence: [],
      score: 0,
      rank: Number.POSITIVE_INFINITY,
      forced: false,
    };

    candidate.sources.add(route.mode);
    candidate.evidence.push({
      fragment: hit.fragment,
      path: route.mode,
      routeId: route.id,
      query: route.query,
      rank,
      score: hit.score,
    });
    addOrUpdateRecall(candidate, {
      path: route.mode,
      routeId: route.id,
      query: route.query,
      found: true,
      rank,
      score: hit.score,
    });
    candidates.set(candidate.id, candidate);
  }
}

function collectAdaptiveRecall(input: {
  routes: readonly ResolvedSearchPlanRoute[];
  filter?: StorageSearchFilter;
  preferSymbol: boolean;
  vectorByRoute: ReadonlyMap<string, readonly number[]>;
  limit: number;
  storage: WorkspaceIndexStorage;
  candidates: Map<string, Candidate>;
}): number {
  const routes = buildRecallRoutes(
    input.routes,
    input.filter,
    input.preferSymbol,
  );
  const targetCandidates = recallTargetCandidateCount(input.limit);
  let previousDepth = 0;
  let depth = RECALL_INITIAL_DEPTH;

  while (true) {
    const saturated = collectRecallPass({
      routes,
      vectorByRoute: input.vectorByRoute,
      depth,
      previousDepth,
      storage: input.storage,
      candidates: input.candidates,
    });

    if (
      input.candidates.size >= targetCandidates ||
      !saturated ||
      depth >= RECALL_MAX_DEPTH
    ) {
      return depth;
    }

    previousDepth = depth;
    depth = Math.min(depth * RECALL_GROWTH_FACTOR, RECALL_MAX_DEPTH);
  }
}

function buildRecallRoutes(
  routes: readonly ResolvedSearchPlanRoute[],
  filter: StorageSearchFilter | undefined,
  preferSymbol: boolean,
): RecallRoute[] {
  const output: RecallRoute[] = routes.map((route) => ({
    ...route,
    filter,
    vectorRouteId: route.mode === "vector" ? route.id : undefined,
  }));

  if (!preferSymbol) {
    return output;
  }

  const seen = new Set<string>();
  for (const route of routes) {
    const symbolNames = extractSymbolNames(route.query);
    if (symbolNames.length === 0) {
      continue;
    }

    const key = `${route.id}\0${symbolNames.join("\0")}`;
    if (seen.has(key)) {
      continue;
    }

    seen.add(key);
    output.push({
      id: `${route.id}.prefer-symbol`,
      mode: "fts",
      query: route.query,
      filter: {
        ...filter,
        symbolNames,
      },
    });
  }

  return output;
}

function collectRecallPass(input: {
  routes: readonly RecallRoute[];
  vectorByRoute: ReadonlyMap<string, readonly number[]>;
  depth: number;
  previousDepth: number;
  storage: WorkspaceIndexStorage;
  candidates: Map<string, Candidate>;
}): boolean {
  let saturated = false;

  for (const route of input.routes) {
    const hits = recallRouteHits(route, input);
    saturated = saturated || hits.length >= input.depth;
    addRecallHits(input.candidates, hits, route, input.storage, {
      startIndex: input.previousDepth,
    });
  }

  return saturated;
}

function recallRouteHits(
  route: RecallRoute,
  input: {
    vectorByRoute: ReadonlyMap<string, readonly number[]>;
    depth: number;
    storage: WorkspaceIndexStorage;
  },
): StorageSearchHit[] {
  if (route.mode === "fts") {
    return input.storage.searchFts(route.query, input.depth, route.filter);
  }

  const vector = input.vectorByRoute.get(route.vectorRouteId ?? route.id);
  return vector
    ? input.storage.searchVector(vector, input.depth, route.filter)
    : [];
}

function recallTargetCandidateCount(limit: number): number {
  return Math.max(limit * RECALL_TARGET_FACTOR, RECALL_MIN_TARGET_CANDIDATES);
}

function resolveHitEntity(
  hit: StorageSearchHit,
  storage: WorkspaceIndexStorage,
): { entity: Entity; file: FileInfo } | null {
  if (!hit.fragment.group || hit.fragment.group === hit.fragment.id) {
    return {
      entity: fragmentToEntity(hit.fragment),
      file: hit.file,
    };
  }

  return storage.getEntity(hit.fragment.group);
}

function fragmentToEntity(fragment: EntityFragment): Entity {
  return {
    id: publicEntityId(fragment),
    fileId: fragment.fileId,
    range: fragment.range,
    content: fragment.content,
    metadata: fragment.metadata,
  };
}

function publicEntityId(fragment: EntityFragment): string {
  return fragment.group ?? fragment.id;
}

function addOrUpdateRecall(
  candidate: Candidate,
  recall: SearchRecallTrace,
): void {
  const existing = candidate.recall.find(
    (item) => item.path === recall.path && item.routeId === recall.routeId,
  );

  if (!existing) {
    candidate.recall.push(recall);
    return;
  }

  if (!recall.found) {
    return;
  }

  if (
    !existing.found ||
    existing.rank === undefined ||
    (recall.rank !== undefined && recall.rank < existing.rank)
  ) {
    Object.assign(existing, recall, {
      forced: existing.forced || recall.forced || undefined,
    });
    return;
  }

  if (recall.forced) {
    existing.forced = true;
  }
}

function extractSymbolNames(query: string): string[] {
  const keywords = new Set([
    "class",
    "struct",
    "enum",
    "interface",
    "function",
    "method",
    "type",
    "const",
    "let",
    "var",
    "namespace",
    "where",
    "find",
    "explain",
  ]);
  const names = new Set<string>();

  for (const match of query.matchAll(/[A-Za-z_~][A-Za-z0-9_:~]*/g)) {
    const token = match[0];
    if (keywords.has(token.toLowerCase())) {
      continue;
    }

    const name = symbolNameFromToken(token);
    if (name) {
      names.add(name);
    }
  }

  return [...names];
}

function symbolNameFromToken(token: string): string | null {
  const parts = token.split("::").filter((part) => part.length > 0);
  const name = parts[parts.length - 1] ?? token;

  if (!/^[A-Za-z_~][A-Za-z0-9_~]*$/.test(name)) {
    return null;
  }

  if (parts.length < 2) {
    return name;
  }

  const owner = parts[parts.length - 2] ?? "";
  if (/^[A-Z_~]/.test(owner)) {
    return `${owner}::${name}`;
  }

  return name;
}

function forceTrackEntity(input: {
  entityId: string;
  routes: readonly ResolvedSearchPlanRoute[];
  vectorByRoute: ReadonlyMap<string, readonly number[]>;
  recallDepth: number;
  filter?: StorageSearchFilter;
  storage: WorkspaceIndexStorage;
  candidates: Map<string, Candidate>;
}): void {
  const tracked = input.storage.getEntity(input.entityId);
  if (!tracked) {
    return;
  }

  const candidate = input.candidates.get(input.entityId) ?? {
    id: input.entityId,
    entity: tracked.entity,
    file: tracked.file,
    sources: new Set<SearchSource>(),
    recall: [],
    evidence: [],
    score: 0,
    rank: Number.POSITIVE_INFINITY,
    forced: true,
  };

  const seenRoutes = new Set(candidate.recall.map((trace) => trace.routeId));

  for (const route of input.routes) {
    if (seenRoutes.has(route.id)) {
      continue;
    }

    if (filterMatchesNoFiles(input.filter)) {
      forceTrackNoMatchingFilesRoute(candidate, route);
      continue;
    }

    if (filterExcludesFile(input.filter, tracked.file.id)) {
      forceTrackPathExcludedRoute(candidate, route);
      continue;
    }

    if (route.mode === "fts") {
      forceTrackFtsRoute(
        candidate,
        {
          ...input,
          targetFileId: tracked.file.id,
        },
        route,
      );
    } else {
      forceTrackVectorRoute(
        candidate,
        {
          ...input,
          targetFileId: tracked.file.id,
        },
        route,
      );
    }
  }

  input.candidates.set(input.entityId, candidate);
}

function filterExcludesFile(
  filter: StorageSearchFilter | undefined,
  fileId: string,
): boolean {
  return filter?.fileIds !== undefined && !filter.fileIds.includes(fileId);
}

function forceTrackNoMatchingFilesRoute(
  candidate: Candidate,
  route: ResolvedSearchPlanRoute,
): void {
  candidate.recall.push({
    path: route.mode,
    routeId: route.id,
    query: route.query,
    found: false,
    forced: true,
    reason: "No files matched the path filters",
  });
}

function forceTrackPathExcludedRoute(
  candidate: Candidate,
  route: ResolvedSearchPlanRoute,
): void {
  candidate.recall.push({
    path: route.mode,
    routeId: route.id,
    query: route.query,
    found: false,
    forced: true,
    reason: "Target entity file was excluded by the path filters",
  });
}

function forceTrackFtsRoute(
  candidate: Candidate,
  input: {
    entityId: string;
    targetFileId: string;
    recallDepth: number;
    filter?: StorageSearchFilter;
    storage: WorkspaceIndexStorage;
  },
  route: ResolvedSearchPlanRoute,
): void {
  const hit = searchTrackedEntityFts(input, route);

  if (hit) {
    candidate.sources.add("fts");
    candidate.evidence.push({
      fragment: hit.fragment,
      path: "fts",
      routeId: route.id,
      query: route.query,
      rank: input.recallDepth + 1,
      score: hit.score,
      forced: true,
    });
    addOrUpdateRecall(candidate, {
      path: "fts",
      routeId: route.id,
      query: route.query,
      found: true,
      forced: true,
      rank: input.recallDepth + 1,
      score: hit.score,
    });
    return;
  }

  candidate.recall.push({
    path: "fts",
    routeId: route.id,
    query: route.query,
    found: false,
    forced: true,
    reason: "Target entity did not match the FTS query",
  });
}

function forceTrackVectorRoute(
  candidate: Candidate,
  input: {
    entityId: string;
    targetFileId: string;
    vectorByRoute: ReadonlyMap<string, readonly number[]>;
    recallDepth: number;
    filter?: StorageSearchFilter;
    storage: WorkspaceIndexStorage;
  },
  route: ResolvedSearchPlanRoute,
): void {
  const vector = input.vectorByRoute.get(route.id);

  if (!vector) {
    candidate.recall.push({
      path: "vector",
      routeId: route.id,
      query: route.query,
      found: false,
      forced: true,
      reason: "Vector route was not available for this query",
    });
    return;
  }

  const hit = searchTrackedEntityVector(input, vector);

  if (hit) {
    candidate.sources.add("vector");
    candidate.evidence.push({
      fragment: hit.fragment,
      path: "vector",
      routeId: route.id,
      query: route.query,
      rank: input.recallDepth + 1,
      score: hit.score,
      forced: true,
    });
    addOrUpdateRecall(candidate, {
      path: "vector",
      routeId: route.id,
      query: route.query,
      found: true,
      forced: true,
      rank: input.recallDepth + 1,
      score: hit.score,
    });
    return;
  }

  candidate.recall.push({
    path: "vector",
    routeId: route.id,
    query: route.query,
    found: false,
    forced: true,
    reason: "Target entity could not be scored by vector search",
  });
}

function searchTrackedEntityFts(
  input: {
    entityId: string;
    targetFileId: string;
    recallDepth: number;
    filter?: StorageSearchFilter;
    storage: WorkspaceIndexStorage;
  },
  route: ResolvedSearchPlanRoute,
): StorageSearchHit | undefined {
  const [groupHit] = input.storage.searchFts(route.query, 1, {
    ...input.filter,
    groupIds: [input.entityId],
  });

  if (groupHit) {
    return groupHit;
  }

  return input.storage
    .searchFts(
      route.query,
      input.recallDepth,
      restrictFilterToFile(input.filter, input.targetFileId),
    )
    .find((hit) => publicEntityId(hit.fragment) === input.entityId);
}

function searchTrackedEntityVector(
  input: {
    entityId: string;
    targetFileId: string;
    recallDepth: number;
    filter?: StorageSearchFilter;
    storage: WorkspaceIndexStorage;
  },
  vector: readonly number[],
): StorageSearchHit | undefined {
  const [groupHit] = input.storage.searchVector(vector, 1, {
    ...input.filter,
    groupIds: [input.entityId],
  });

  if (groupHit) {
    return groupHit;
  }

  return input.storage
    .searchVector(
      vector,
      input.recallDepth,
      restrictFilterToFile(input.filter, input.targetFileId),
    )
    .find((hit) => publicEntityId(hit.fragment) === input.entityId);
}

function restrictFilterToFile(
  filter: StorageSearchFilter | undefined,
  fileId: string,
): StorageSearchFilter {
  return {
    ...filter,
    fileIds: [fileId],
  };
}

function searchPlanToStorageFilter(
  plan: SearchPlan,
  storage: WorkspaceIndexStorage,
  fileTypePatterns: FileTypePatterns,
): StorageSearchFilter | undefined {
  const fileIds = resolveFilteredFileIds(
    plan,
    storage.listFiles(),
    fileTypePatterns,
  );
  const symbolTypes =
    plan.symbolTypes && plan.symbolTypes.length > 0
      ? plan.symbolTypes
      : undefined;

  if (fileIds === undefined && symbolTypes === undefined) {
    return undefined;
  }

  return {
    ...(fileIds !== undefined ? { fileIds } : {}),
    ...(symbolTypes !== undefined ? { symbolTypes } : {}),
  };
}

function filterMatchesNoFiles(
  filter: StorageSearchFilter | undefined,
): boolean {
  return filter?.fileIds !== undefined && filter.fileIds.length === 0;
}

function resolveFilteredFileIds(
  plan: SearchPlan,
  files: readonly FileInfo[],
  fileTypePatterns: FileTypePatterns,
): string[] | undefined {
  const includeMatchers = (plan.includePaths ?? []).map(compilePathFilter);
  const excludeMatchers = (plan.excludePaths ?? []).map(compilePathFilter);
  const hasModifiedFilter =
    plan.modifiedAfter !== undefined || plan.modifiedBefore !== undefined;
  const hasSharedSelection =
    (plan.globs?.length ?? 0) > 0 ||
    (plan.insensitiveGlobs?.length ?? 0) > 0 ||
    fileTypePatterns.include.length > 0 ||
    fileTypePatterns.exclude.length > 0;

  if (
    includeMatchers.length === 0 &&
    excludeMatchers.length === 0 &&
    !hasModifiedFilter &&
    !hasSharedSelection
  ) {
    return undefined;
  }

  return files
    .filter((file) => {
      const included =
        includeMatchers.length === 0 ||
        includeMatchers.some((matcher) => matcher(file));
      const excluded = excludeMatchers.some((matcher) => matcher(file));

      return (
        included &&
        !excluded &&
        matchesFileSelection(file.relativePath, plan, fileTypePatterns) &&
        matchesModifiedTimeFilter(file, plan)
      );
    })
    .map((file) => file.id);
}

function matchesModifiedTimeFilter(file: FileInfo, plan: SearchPlan): boolean {
  if (
    plan.modifiedAfter !== undefined &&
    file.lastModifiedTime < plan.modifiedAfter
  ) {
    return false;
  }

  if (
    plan.modifiedBefore !== undefined &&
    file.lastModifiedTime > plan.modifiedBefore
  ) {
    return false;
  }

  return true;
}

function compilePathFilter(pattern: string): PathFilterMatcher {
  const pathTarget = isAbsolutePathPattern(pattern)
    ? "absolutePath"
    : "relativePath";

  if (hasPathGlob(pattern)) {
    return (file) =>
      pathPatternMatches(pattern, normalizePathForMatch(file[pathTarget]));
  }

  return (file) => {
    const path = normalizePathForMatch(file[pathTarget]);

    return pathPatternMatches(pattern, path);
  };
}

function normalizePathFilterPattern(pattern: string): string {
  return normalizePathPattern(pattern);
}

function fuseCandidates(candidates: Map<string, Candidate>): Candidate[] {
  for (const candidate of candidates.values()) {
    candidate.score = 0;
    candidate.forced = candidate.recall.some((trace) => trace.forced);

    for (const recall of candidate.recall) {
      if (recall.found && recall.rank !== undefined) {
        candidate.score += 1 / (RRF_K + recall.rank);
      }
    }
  }

  const fused = [...candidates.values()].sort((left, right) => {
    if (right.score !== left.score) {
      return right.score - left.score;
    }

    return left.id.localeCompare(right.id);
  });

  for (const [index, candidate] of fused.entries()) {
    candidate.rank = index + 1;
  }

  return fused;
}

function candidateToHit(
  candidate: Candidate,
  limit: number,
  trace: boolean,
): SearchHit {
  return {
    entity: candidate.entity,
    file: candidate.file,
    evidence: sortEvidence(candidate.evidence).map(evidenceToSearchHitEvidence),
    rank: candidate.rank,
    score: candidate.score,
    matchedBy: deriveMatchedBy(candidate.sources),
    trace: trace ? candidateToTrace(candidate, limit) : undefined,
  };
}

function evidenceToSearchHitEvidence(
  evidence: InternalSearchEvidence,
): SearchHitEvidence {
  return {
    range: evidence.fragment.range,
    content: evidence.fragment.content,
    metadata: evidence.fragment.metadata,
    isEntity: evidence.fragment.id === publicEntityId(evidence.fragment),
    path: evidence.path,
    routeId: evidence.routeId,
    query: evidence.query,
    rank: evidence.rank,
    score: evidence.score,
    forced: evidence.forced,
  };
}

function sortEvidence(
  evidence: readonly InternalSearchEvidence[],
): InternalSearchEvidence[] {
  return [...evidence].sort((left, right) => {
    const leftRank = left.rank ?? Number.POSITIVE_INFINITY;
    const rightRank = right.rank ?? Number.POSITIVE_INFINITY;

    if (leftRank !== rightRank) {
      return leftRank - rightRank;
    }

    if (left.path !== right.path) {
      return left.path.localeCompare(right.path);
    }

    return left.fragment.id.localeCompare(right.fragment.id);
  });
}

function candidateToTrace(candidate: Candidate, limit: number): SearchHitTrace {
  const final: SearchFinalTrace = {
    returnedByLimit: candidate.rank <= limit,
    cutoffRank: limit,
  };

  return {
    recall: candidate.recall,
    fusion: {
      rank: candidate.rank,
      score: candidate.score,
      forced: candidate.forced || undefined,
    },
    final,
  };
}

async function chooseBestEntityInFile(
  query: string,
  file: FileInfo,
  ctx: SearchContext,
): Promise<string | null> {
  const candidates = new Map<string, Candidate>();
  addRecallHits(
    candidates,
    ctx.storage.searchFts(query, 10, {
      fileIds: [file.id],
    }),
    {
      id: "fts",
      mode: "fts",
      query,
    },
    ctx.storage,
  );

  const {
    vectors: [queryVector],
  } = await requireEmbeddingModel(ctx, "diagnose").embed(
    [{ kind: "text", text: query }],
    { purpose: "query" },
  );
  addRecallHits(
    candidates,
    ctx.storage.searchVector(queryVector, 10, {
      fileIds: [file.id],
    }),
    {
      id: "vector",
      mode: "vector",
      query,
    },
    ctx.storage,
  );

  const [best] = fuseCandidates(candidates);
  if (best) {
    return best.id;
  }

  const [first] = ctx.storage.listEntitiesByFile(file.id, { limit: 1 });

  return first?.entity.id ?? null;
}

function deriveMatchedBy(sources: Set<SearchSource>): SearchMatchedBy {
  if (sources.has("fts") && sources.has("vector")) {
    return "fts+vector";
  }
  if (sources.has("fts")) {
    return "fts";
  }
  if (sources.has("vector")) {
    return "vector";
  }
  return "graph";
}

function expandGraphNeighbors(input: {
  graph: GraphReader | undefined;
  storage: WorkspaceIndexStorage;
  candidates: Map<string, Candidate>;
  seeds: readonly Candidate[];
  limit: number;
  filter?: StorageSearchFilter;
}): {
  available: boolean;
  unavailableReason?: string;
  seeds: number;
  neighborsAdded: number;
  rerankNeeded: boolean;
  seedIds: string[];
  relations: SearchGraphRelation[];
} {
  if (!input.graph?.available) {
    return {
      available: false,
      unavailableReason: input.graph?.unavailableReason,
      seeds: 0,
      neighborsAdded: 0,
      rerankNeeded: false,
      seedIds: [],
      relations: [],
    };
  }

  const seedTopK = Math.min(
    GRAPH_EXPAND_SEED_TOP_K,
    Math.max(1, input.limit),
    input.seeds.length,
  );
  const seeds = input.seeds.slice(0, seedTopK);
  if (seeds.length === 0) {
    return {
      available: true,
      seeds: 0,
      neighborsAdded: 0,
      rerankNeeded: false,
      seedIds: [],
      relations: [],
    };
  }

  const subgraph = exploreSubgraph(input.graph, input.storage, {
    seedIds: seeds.map((seed) => seed.id),
    seedWeights: new Map(
      seeds.map((seed) => [seed.id, 1 / (RRF_K + seed.rank)]),
    ),
    traversalDepth: GRAPH_EXPAND_DEPTH,
    maxNodes: GRAPH_EXPAND_MAX_NODES,
    includeCallPaths: false,
  });
  const connectedNodeIds = new Set(
    subgraph.edges.flatMap((edge) => [edge.src, edge.dst]),
  );
  const ranked = subgraph.nodes
    .filter(
      (node) =>
        connectedNodeIds.has(node.id) &&
        node.entity &&
        matchesGraphFilter(node.entity, input.filter),
    )
    .sort((a, b) => {
      const scoreDiff =
        (subgraph.nodeScores.get(b.id) ?? 0) -
        (subgraph.nodeScores.get(a.id) ?? 0);
      return scoreDiff || a.id.localeCompare(b.id);
    });
  const relations = subgraph.edges
    .map((edge) => symbolSearchRelation(input.storage, edge))
    .filter((relation): relation is SearchGraphRelation => relation !== null);

  let neighborsAdded = 0;
  let rerankNeeded = false;
  for (let index = 0; index < ranked.length; index++) {
    const node = ranked[index]!;
    const existed = input.candidates.has(node.id);
    if (
      mergeGraphCandidate({
        storage: input.storage,
        candidates: input.candidates,
        entityId: node.id,
        routeId: "graph.explore",
        syntheticRank: GRAPH_EXPAND_RANK_PENALTY + index + 1,
        score: subgraph.nodeScores.get(node.id) ?? 0,
      })
    ) {
      rerankNeeded = true;
      if (!existed) neighborsAdded += 1;
    }
  }
  const importExpansion = injectImportFileNeighbors({
    graph: input.graph,
    storage: input.storage,
    candidates: input.candidates,
    seeds,
    filter: input.filter,
    relations,
  });
  neighborsAdded += importExpansion.added;
  rerankNeeded = rerankNeeded || importExpansion.changed;

  return {
    available: true,
    seeds: seeds.length,
    neighborsAdded,
    rerankNeeded,
    seedIds: seeds.map((seed) => seed.id),
    relations,
  };
}

function injectImportFileNeighbors(input: {
  graph: GraphReader;
  storage: WorkspaceIndexStorage;
  candidates: Map<string, Candidate>;
  seeds: readonly Candidate[];
  filter?: StorageSearchFilter;
  relations: SearchGraphRelation[];
}): { added: number; changed: boolean } {
  const seedFileRank = new Map<string, number>();
  for (const seed of input.seeds) {
    const existing = seedFileRank.get(seed.file.id);
    if (existing === undefined || seed.rank < existing) {
      seedFileRank.set(seed.file.id, seed.rank);
    }
  }
  const neighbors = input.graph.expandFileNeighbors(
    [...seedFileRank.keys()],
    GRAPH_EXPAND_FILE_PER_SEED,
  );
  const seenFiles = new Set<string>();
  let added = 0;
  let changed = false;
  for (const neighbor of neighbors) {
    if (seedFileRank.has(neighbor.id) || seenFiles.has(neighbor.id)) continue;
    seenFiles.add(neighbor.id);
    const entities = pickImportNeighborEntities(
      input.storage.listEntitiesByFile(neighbor.id, {
        limit: GRAPH_EXPAND_ENTITIES_PER_FILE * 4,
      }),
      GRAPH_EXPAND_ENTITIES_PER_FILE,
    );
    const seed = input.seeds.find((item) => item.file.id === neighbor.fid);
    const neighborFile = entities[0]?.file;
    if (seed && neighborFile) {
      const source = neighbor.direction === "out" ? seed.file : neighborFile;
      const destination =
        neighbor.direction === "out" ? neighborFile : seed.file;
      input.relations.push({
        srcId: source.id,
        dstId: destination.id,
        srcLabel: source.relativePath,
        dstLabel: destination.relativePath,
        kind: "IMPORTS",
        scope: "file",
      });
    }
    for (const stored of entities) {
      if (!matchesGraphFilter(stored, input.filter)) continue;
      const existed = input.candidates.has(stored.entity.id);
      if (
        mergeGraphCandidate({
          storage: input.storage,
          candidates: input.candidates,
          entityId: stored.entity.id,
          routeId: "graph.explore.imports",
          syntheticRank:
            (seedFileRank.get(neighbor.fid) ?? input.seeds.length) +
            GRAPH_EXPAND_IMPORT_RANK_PENALTY,
          score: 1,
        })
      ) {
        changed = true;
        if (!existed) added += 1;
      }
    }
  }
  return { added, changed };
}

function pickImportNeighborEntities(
  entities: readonly StoredEntity[],
  limit: number,
): StoredEntity[] {
  const exported = entities.filter((item) => {
    const metadata = item.entity.metadata;
    return (
      metadata?.kind === "code" &&
      metadata.modifiers.includes("exported") &&
      (metadata.symbolType === "function" ||
        metadata.symbolType === "class" ||
        metadata.symbolType === "interface")
    );
  });
  const code = entities.filter((item) => item.entity.metadata?.kind === "code");
  return (
    exported.length > 0 ? exported : code.length > 0 ? code : entities
  ).slice(0, limit);
}

function mergeGraphCandidate(input: {
  storage: WorkspaceIndexStorage;
  candidates: Map<string, Candidate>;
  entityId: string;
  routeId: string;
  syntheticRank: number;
  score: number;
}): boolean {
  const stored = input.storage.getEntity(input.entityId);
  if (!stored) {
    return false;
  }

  const fragment: EntityFragment = {
    id: stored.entity.id,
    fileId: stored.entity.fileId,
    range: stored.entity.range,
    content: stored.entity.content,
    metadata: stored.entity.metadata,
  };

  const existing = input.candidates.get(input.entityId);
  if (existing) {
    existing.sources.add("graph");
    existing.evidence.push({
      fragment,
      path: "graph",
      routeId: input.routeId,
      rank: input.syntheticRank,
      score: input.score,
    });
    addOrUpdateRecall(existing, {
      path: "graph",
      routeId: input.routeId,
      found: true,
      rank: input.syntheticRank,
      score: input.score,
    });
    return true;
  }

  input.candidates.set(input.entityId, {
    id: input.entityId,
    entity: stored.entity,
    file: stored.file,
    sources: new Set<SearchSource>(["graph"]),
    recall: [
      {
        path: "graph",
        routeId: input.routeId,
        found: true,
        rank: input.syntheticRank,
        score: input.score,
      },
    ],
    evidence: [
      {
        fragment,
        path: "graph",
        routeId: input.routeId,
        rank: input.syntheticRank,
        score: input.score,
      },
    ],
    score: 0,
    rank: 0,
    forced: false,
  });
  return true;
}

function matchesGraphFilter(
  stored: StoredEntity,
  filter: StorageSearchFilter | undefined,
): boolean {
  if (!filter) return true;
  if (filter.fileIds && !filter.fileIds.includes(stored.file.id)) return false;
  if (filter.groupIds && !filter.groupIds.includes(stored.entity.id)) {
    return false;
  }
  const metadata = stored.entity.metadata;
  const symbolType =
    metadata?.kind === "code" ? metadata.symbolType : undefined;
  if (
    filter.symbolTypes &&
    (!symbolType || !filter.symbolTypes.includes(symbolType))
  ) {
    return false;
  }
  const symbolName =
    metadata?.kind === "code" ? metadata.symbolName : undefined;
  if (
    filter.symbolNames &&
    (!symbolName || !filter.symbolNames.includes(symbolName))
  ) {
    return false;
  }
  return true;
}

function symbolSearchRelation(
  storage: WorkspaceIndexStorage,
  edge: { src: string; dst: string; kind: GraphEdgeKind },
): SearchGraphRelation | null {
  if (edge.kind === "IMPORTS" || edge.kind === "DEFINES") return null;
  const src = storage.getEntity(edge.src);
  const dst = storage.getEntity(edge.dst);
  if (!src || !dst) return null;
  return {
    srcId: edge.src,
    dstId: edge.dst,
    srcLabel: storedEntityLabel(src),
    dstLabel: storedEntityLabel(dst),
    kind: edge.kind,
    scope: "symbol",
  };
}

function storedEntityLabel(stored: StoredEntity): string {
  const metadata = stored.entity.metadata;
  return metadata?.kind === "code" && metadata.symbolName
    ? metadata.symbolName
    : stored.entity.id;
}

function selectGraphRelationships(
  relations: readonly SearchGraphRelation[],
  visible: readonly Candidate[],
  seedIds: readonly string[],
  candidates: ReadonlyMap<string, Candidate>,
): SearchGraphRelation[] {
  const visibleEntities = new Set(visible.map((candidate) => candidate.id));
  const visibleFiles = new Set(visible.map((candidate) => candidate.file.id));
  const allowedEntities = new Set([...visibleEntities, ...seedIds]);
  const allowedFiles = new Set(visibleFiles);
  for (const id of seedIds) {
    const seed = candidates.get(id);
    if (seed) allowedFiles.add(seed.file.id);
  }
  const seen = new Set<string>();
  const priority: Record<SearchGraphRelation["kind"], number> = {
    CALLS: 0,
    INHERITS: 1,
    CONTAINS: 2,
    REFS: 3,
    IMPORTS: 4,
    INSTANTIATES: 2,
  };
  return relations
    .filter((relation) => {
      const allowed =
        relation.scope === "symbol" ? allowedEntities : allowedFiles;
      const visibleSet =
        relation.scope === "symbol" ? visibleEntities : visibleFiles;
      if (!allowed.has(relation.srcId) || !allowed.has(relation.dstId))
        return false;
      if (!visibleSet.has(relation.srcId) && !visibleSet.has(relation.dstId))
        return false;
      const key = `${relation.scope}\0${relation.srcId}\0${relation.dstId}\0${relation.kind}`;
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    })
    .sort((a, b) => priority[a.kind] - priority[b.kind])
    .slice(0, 20);
}
