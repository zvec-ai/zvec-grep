import type { GraphQueryStorage } from "../ports.js";
import type {
  DynamicBoundary,
  GraphEdgeKind,
  GraphReader,
  SymRef,
} from "../types.js";
import { personalizedPageRank } from "../application/ranking.js";
import { collectCallPaths } from "./paths.js";
import { assembleExploreFiles } from "./assembly.js";
import {
  collectBlastRadius,
  collectChangeSurface,
  fileIdsForRoots,
  includeChangeSurfaceNodes,
} from "./impact.js";
import { EXPLORE_POLICY, queryTerms, resolveExploreSeeds } from "./policy.js";
import type {
  ExploreEdge,
  ExploreNode,
  ExploreOptions,
  ExploreResult,
  ExploreSubgraphOptions,
  ExploreSubgraphResult,
  ExploreSubgraphStorage,
} from "./types.js";

const DEFAULT_SEARCH_LIMIT = EXPLORE_POLICY.searchLimit;
const DEFAULT_TRAVERSAL_DEPTH = EXPLORE_POLICY.traversalDepth;
const DEFAULT_MAX_NODES = EXPLORE_POLICY.maxNodes;
const DEFAULT_MAX_FILES = EXPLORE_POLICY.maxFiles;
const DEFAULT_MAX_CHARS = EXPLORE_POLICY.maxChars;
const DEFAULT_GLUE_LIMIT = EXPLORE_POLICY.glueLimit;
const DEFAULT_CONTAINER_GLUE_LIMIT = EXPLORE_POLICY.containerGlueLimit;
const DEFAULT_PATH_LIMIT = EXPLORE_POLICY.pathLimit;
const DEFAULT_BLAST_LIMIT = EXPLORE_POLICY.blastLimit;
const HIERARCHY_BUDGET_RATIO = EXPLORE_POLICY.hierarchyBudgetRatio;
const RWR_EDGE_WEIGHTS = EXPLORE_POLICY.rwrEdgeWeights;
const TRAVERSE_EDGE_KINDS: readonly GraphEdgeKind[] =
  EXPLORE_POLICY.traverseEdgeKinds;

type ScoredNode = {
  id: string;
  kind?: string;
  isRoot: boolean;
  depth: number;
};

/**
 * CodeGraph-style explore: seed → hierarchy → deep traverse → RWR file rank →
 * zvec entity-content assembly (no graph-layer disk reads).
 */
export function exploreGraph(
  graph: GraphReader,
  storage: GraphQueryStorage,
  options: ExploreOptions,
): ExploreResult {
  const query = options.query.trim();
  const searchLimit = clampInt(
    options.searchLimit ?? DEFAULT_SEARCH_LIMIT,
    1,
    32,
  );
  const traversalDepth = clampInt(
    options.traversalDepth ?? DEFAULT_TRAVERSAL_DEPTH,
    1,
    8,
  );
  const maxNodes = clampInt(options.maxNodes ?? DEFAULT_MAX_NODES, 16, 2_000);
  const maxFiles = clampInt(options.maxFiles ?? DEFAULT_MAX_FILES, 1, 32);
  const maxChars = clampInt(
    options.maxChars ?? DEFAULT_MAX_CHARS,
    1_000,
    200_000,
  );

  if (!graph.available) {
    return emptyResult(query, "graph_unavailable");
  }
  if (!query && !options.seedId) {
    throw new Error("explore requires a query or seedId");
  }

  const rootIds = resolveExploreSeeds(
    storage,
    query,
    options.seedId,
    searchLimit,
  );
  if (rootIds.length === 0) {
    return emptyResult(query, "no_seeds");
  }

  const subgraph = exploreSubgraph(graph, storage, {
    seedIds: rootIds,
    traversalDepth,
    maxNodes,
    includeCallPaths: true,
  });
  const { nodes, edges, edgesTruncated, callPaths } = subgraph;
  const dynamicBoundaryLimit = Math.min(100, maxNodes);
  const dynamicBoundaryRows =
    graph.dynamicBoundaries?.(
      nodes.map((node) => node.id),
      dynamicBoundaryLimit + 1,
    ) ?? [];
  const dynamicBoundaries = selectDynamicBoundaries(
    dynamicBoundaryRows,
    dynamicBoundaryLimit,
  );
  const dynamicBoundariesTruncated =
    dynamicBoundaryRows.length > dynamicBoundaries.length;
  const blastRadius = collectBlastRadius(
    graph,
    storage,
    rootIds,
    DEFAULT_BLAST_LIMIT,
  );
  const fileScores = rankFilesWithRwr(
    nodes,
    rootIds,
    query,
    subgraph.nodeScores,
  );
  const changeSurface = collectChangeSurface({
    graph,
    storage,
    rootIds,
    nodes,
    nodeScores: subgraph.nodeScores,
    fileScores,
    query,
    maxFiles,
  });
  const assemblyNodes = includeChangeSurfaceNodes(nodes, changeSurface);
  const files = assembleExploreFiles({
    storage,
    nodes: assemblyNodes,
    fileScores,
    maxFiles,
    maxChars,
    rootFileIds: fileIdsForRoots(nodes, rootIds),
    changeSurfaceFileIds: new Set(
      changeSurface
        .filter((item) => item.rescued)
        .map((item) => item.entity.file.id),
    ),
  });

  if (files.length === 0) {
    return {
      available: true,
      query,
      roots: nodes.filter((n) => n.isRoot),
      nodes,
      edges,
      edgesTruncated,
      callPaths,
      blastRadius,
      changeSurface,
      dynamicBoundaries,
      dynamicBoundariesTruncated,
      files: [],
      emptyReason: "no_context",
    };
  }

  return {
    available: true,
    query,
    roots: nodes.filter((n) => n.isRoot),
    nodes,
    edges,
    edgesTruncated,
    callPaths,
    blastRadius,
    changeSurface,
    dynamicBoundaries,
    dynamicBoundariesTruncated,
    files,
  };
}

/** Keep candidate-less receiver calls visible without letting them dominate context. */
function selectDynamicBoundaries(
  boundaries: readonly DynamicBoundary[],
  limit: number,
): DynamicBoundary[] {
  const uncertainLimit = Math.min(12, Math.max(1, Math.floor(limit / 4)));
  const selected: DynamicBoundary[] = [];
  let uncertainCount = 0;
  for (const boundary of boundaries) {
    if (boundary.candidateDetails.length === 0) {
      if (uncertainCount >= uncertainLimit) continue;
      uncertainCount++;
    }
    selected.push(boundary);
    if (selected.length >= limit) break;
  }
  return selected;
}

/**
 * Shared graph expansion used by both explore and ordinary search. It builds
 * and scores a bounded multi-seed subgraph without assembling source bundles
 * or calculating blast radius.
 */
export function exploreSubgraph(
  graph: GraphReader,
  storage: ExploreSubgraphStorage,
  options: ExploreSubgraphOptions,
): ExploreSubgraphResult {
  if (!graph.available) {
    return emptySubgraph(false);
  }
  const traversalDepth = clampInt(
    options.traversalDepth ?? DEFAULT_TRAVERSAL_DEPTH,
    1,
    8,
  );
  const maxNodes = clampInt(options.maxNodes ?? DEFAULT_MAX_NODES, 16, 2_000);
  const rootIds = [...new Set(options.seedIds)]
    .filter((id) => Boolean(storage.getEntity(id)))
    .slice(0, maxNodes);
  if (rootIds.length === 0) {
    return emptySubgraph(true);
  }

  const selected = new Map<string, ScoredNode>();
  for (const id of rootIds) {
    selected.set(id, {
      id,
      kind: undefined,
      isRoot: true,
      depth: 0,
    });
  }

  const hierarchyBudget = Math.max(
    8,
    Math.floor(maxNodes * HIERARCHY_BUDGET_RATIO),
  );
  expandHierarchy(graph, selected, rootIds, hierarchyBudget);
  glueContainers(
    graph,
    selected,
    rootIds,
    Math.min(DEFAULT_CONTAINER_GLUE_LIMIT, maxNodes),
  );

  const perRootBudget = Math.max(
    8,
    Math.ceil(maxNodes / Math.max(1, rootIds.length)),
  );
  for (const rootId of [...selected.keys()].filter(
    (id) => selected.get(id)?.isRoot,
  )) {
    const walked = graph.traverse(rootId, {
      edgeKinds: TRAVERSE_EDGE_KINDS,
      direction: "both",
      maxDepth: traversalDepth,
      limit: perRootBudget,
      includeStart: true,
    });
    for (const ref of walked) {
      absorb(selected, ref, false, 1);
    }
  }

  glueCallNeighbors(graph, selected, rootIds, DEFAULT_GLUE_LIMIT);

  const pathResult =
    options.includeCallPaths === false
      ? { paths: [], refs: [] }
      : collectCallPaths(
          graph,
          rootIds,
          Math.max(4, traversalDepth * 2),
          DEFAULT_PATH_LIMIT,
        );
  for (const ref of pathResult.refs) absorb(selected, ref, false, 1);
  const callPaths = pathResult.paths;

  const protectedIds = new Set([
    ...rootIds,
    ...callPaths.flatMap((path) => path.nodes),
  ]);
  trimToMaxNodes(selected, protectedIds, maxNodes);
  const retainedCallPaths = callPaths.filter((path) =>
    path.nodes.every((id) => selected.has(id)),
  );

  const nodes: ExploreNode[] = [];
  for (const scored of selected.values()) {
    const entity = storage.getEntity(scored.id);
    const metaKind =
      entity?.entity.metadata?.kind === "code"
        ? entity.entity.metadata.symbolType
        : undefined;
    nodes.push({
      id: scored.id,
      kind: scored.kind ?? metaKind,
      isRoot: scored.isRoot,
      entity,
    });
  }

  const edgeBudget = Math.min(20_000, Math.max(128, maxNodes * 8));
  const induced = collectExploreEdges(graph, selected, edgeBudget);
  const edges = induced.edges;
  return {
    available: true,
    rootIds,
    nodes,
    edges,
    edgesTruncated: induced.truncated,
    callPaths: retainedCallPaths,
    nodeScores: rankNodesWithRwr(nodes, edges, rootIds, options.seedWeights),
  };
}

function emptySubgraph(available: boolean): ExploreSubgraphResult {
  return {
    available,
    rootIds: [],
    nodes: [],
    edges: [],
    edgesTruncated: false,
    callPaths: [],
    nodeScores: new Map(),
  };
}

function expandHierarchy(
  graph: GraphReader,
  selected: Map<string, ScoredNode>,
  rootIds: readonly string[],
  budget: number,
): void {
  let remaining = budget;

  for (const rootId of rootIds) {
    if (remaining <= 0) {
      break;
    }
    for (const ref of graph.hierarchy(
      rootId,
      "bases",
      Math.min(10, remaining),
    )) {
      if (absorb(selected, ref, false, 1)) {
        remaining -= 1;
      }
    }
    for (const ref of graph.hierarchy(
      rootId,
      "derived",
      Math.min(10, remaining),
    )) {
      if (absorb(selected, ref, false, 1)) {
        remaining -= 1;
      }
    }
  }

  // Sibling types: other derived types of the same bases.
  const baseIds = new Set<string>();
  for (const id of [...selected.keys()]) {
    for (const base of graph.hierarchy(id, "bases", 5)) {
      baseIds.add(base.id);
      absorb(selected, base, false, 1);
    }
  }
  for (const baseId of baseIds) {
    if (remaining <= 0) {
      break;
    }
    for (const sib of graph.hierarchy(baseId, "derived", 12)) {
      if (absorb(selected, sib, false, 2)) {
        remaining -= 1;
        if (remaining <= 0) {
          break;
        }
      }
    }
  }
}

function glueCallNeighbors(
  graph: GraphReader,
  selected: Map<string, ScoredNode>,
  rootIds: readonly string[],
  limit: number,
): void {
  let added = 0;
  for (const rootId of rootIds) {
    if (added >= limit) {
      break;
    }
    for (const ref of [
      ...graph.callers(rootId, 1, 20),
      ...graph.callees(rootId, 1, 20),
    ]) {
      if (absorb(selected, ref, false, 1)) {
        added += 1;
        if (added >= limit) {
          break;
        }
      }
    }
  }
}

function glueContainers(
  graph: GraphReader,
  selected: Map<string, ScoredNode>,
  rootIds: readonly string[],
  limit: number,
): void {
  let added = 0;
  for (const neighbor of graph.expandContainers(rootIds, limit)) {
    for (const id of [neighbor.parent_id, neighbor.sib_id]) {
      if (!id || added >= limit) continue;
      if (absorb(selected, { id }, false, 1)) added += 1;
    }
  }
}

function trimToMaxNodes(
  selected: Map<string, ScoredNode>,
  protectedIds: ReadonlySet<string>,
  maxNodes: number,
): void {
  if (selected.size <= maxNodes) {
    return;
  }
  const ranked = [...selected.values()].sort((a, b) => {
    const ar = protectedIds.has(a.id) ? 0 : 1;
    const br = protectedIds.has(b.id) ? 0 : 1;
    if (ar !== br) {
      return ar - br;
    }
    if (a.depth !== b.depth) {
      return a.depth - b.depth;
    }
    return a.id.localeCompare(b.id);
  });
  selected.clear();
  for (const node of ranked.slice(0, maxNodes)) {
    selected.set(node.id, node);
  }
}

function collectExploreEdges(
  graph: GraphReader,
  selected: Map<string, ScoredNode>,
  limit: number,
): { edges: ExploreEdge[]; truncated: boolean } {
  const ids = [...selected.keys()];
  const result = graph.edges(ids, TRAVERSE_EDGE_KINDS, limit);
  return {
    edges: result.edges.map((edge) => ({
      src: edge.src,
      dst: edge.dst,
      kind: edge.kind,
      rel: edge.rel,
      count: edge.count,
      firstLine: edge.first_line,
      refName: edge.ref_name,
      provenance: edge.provenance ?? "static",
      confidence: edge.confidence ?? 1,
      evidence: edge.evidence,
    })),
    truncated: result.truncated,
  };
}

function rankFilesWithRwr(
  nodes: readonly ExploreNode[],
  rootIds: readonly string[],
  query: string,
  nodeScores: ReadonlyMap<string, number>,
): Map<string, number> {
  const nodeById = new Map(nodes.map((n) => [n.id, n]));

  const terms = queryTerms(query);
  const fileScores = new Map<string, number>();
  const fileTermHits = new Map<string, number>();

  for (const node of nodes) {
    const fileId = node.entity?.file.id;
    if (!fileId) {
      continue;
    }
    const nodeScore = nodeScores.get(node.id) ?? 0;
    const rootBoost = node.isRoot ? 0.15 : 0;
    fileScores.set(
      fileId,
      (fileScores.get(fileId) ?? 0) + nodeScore + rootBoost,
    );

    if (terms.length > 0) {
      const hay = [
        node.entity?.entity.metadata?.kind === "code"
          ? node.entity.entity.metadata.symbolName
          : "",
        node.entity?.file.relativePath ?? "",
      ]
        .join(" ")
        .toLowerCase();
      let hits = 0;
      for (const term of terms) {
        if (hay.includes(term)) {
          hits += 1;
        }
      }
      if (hits > 0) {
        fileTermHits.set(fileId, Math.max(fileTermHits.get(fileId) ?? 0, hits));
      }
    }
  }

  for (const [fileId, hits] of fileTermHits) {
    fileScores.set(fileId, (fileScores.get(fileId) ?? 0) * (1 + 0.35 * hits));
  }

  // Drop files with no term hits when query has identifiers, unless root file.
  const rootFiles = new Set(
    rootIds
      .map((id) => nodeById.get(id)?.entity?.file.id)
      .filter((id): id is string => typeof id === "string"),
  );
  if (terms.length > 0) {
    for (const fileId of [...fileScores.keys()]) {
      if (rootFiles.has(fileId)) {
        continue;
      }
      if ((fileTermHits.get(fileId) ?? 0) === 0) {
        const score = fileScores.get(fileId) ?? 0;
        const max = Math.max(...fileScores.values(), 0);
        if (score < max * 0.06) {
          fileScores.delete(fileId);
        }
      }
    }
  }

  return fileScores;
}

function rankNodesWithRwr(
  nodes: readonly ExploreNode[],
  edges: readonly ExploreEdge[],
  rootIds: readonly string[],
  seedWeights?: ReadonlyMap<string, number>,
): Map<string, number> {
  const adj = new Map<string, Map<string, number>>();
  for (const node of nodes) {
    adj.set(node.id, new Map());
  }
  for (const edge of edges) {
    if (!adj.has(edge.src) || !adj.has(edge.dst)) {
      continue;
    }
    const weight = RWR_EDGE_WEIGHTS[edge.kind] * (edge.confidence ?? 1);
    addWeightedNeighbor(adj.get(edge.src)!, edge.dst, weight);
    addWeightedNeighbor(adj.get(edge.dst)!, edge.src, weight);
  }
  return personalizedPageRank(
    [...adj.keys()],
    adj,
    rootIds.filter((id) => adj.has(id)),
    seedWeights,
  );
}

function addWeightedNeighbor(
  neighbors: Map<string, number>,
  id: string,
  weight: number,
): void {
  neighbors.set(id, (neighbors.get(id) ?? 0) + weight);
}

function absorb(
  selected: Map<string, ScoredNode>,
  ref: SymRef,
  isRoot: boolean,
  depth: number,
): boolean {
  const existing = selected.get(ref.id);
  if (existing) {
    if (isRoot) {
      existing.isRoot = true;
    }
    existing.depth = Math.min(existing.depth, depth);
    if (ref.kind) {
      existing.kind = ref.kind;
    }
    return false;
  }
  selected.set(ref.id, {
    id: ref.id,
    kind: ref.kind,
    isRoot,
    depth,
  });
  return true;
}

function emptyResult(
  query: string,
  emptyReason: NonNullable<ExploreResult["emptyReason"]>,
): ExploreResult {
  return {
    available: emptyReason !== "graph_unavailable",
    query,
    roots: [],
    nodes: [],
    edges: [],
    edgesTruncated: false,
    callPaths: [],
    blastRadius: [],
    changeSurface: [],
    dynamicBoundaries: [],
    dynamicBoundariesTruncated: false,
    files: [],
    emptyReason,
  };
}

function clampInt(value: number, min: number, max: number): number {
  if (!Number.isFinite(value)) {
    return min;
  }
  return Math.min(max, Math.max(min, Math.floor(value)));
}
