import type { StoredEntity } from "../../storage/index.js";
import type { FileInfo, Range } from "../../types.js";
import type { GraphQueryStorage } from "../ports.js";
import type { DynamicBoundary, GraphEdgeKind } from "../types.js";

export type ExploreOptions = {
  query: string;
  seedId?: string;
  searchLimit?: number;
  traversalDepth?: number;
  maxNodes?: number;
  maxFiles?: number;
  maxChars?: number;
};

export type ExploreSubgraphOptions = {
  seedIds: readonly string[];
  seedWeights?: ReadonlyMap<string, number>;
  traversalDepth?: number;
  maxNodes?: number;
  includeCallPaths?: boolean;
};

export type ExploreSubgraphStorage = Pick<GraphQueryStorage, "getEntity">;

export type ExploreNode = {
  id: string;
  kind?: string;
  isRoot: boolean;
  entity: StoredEntity | null;
};

export type ExploreEdge = {
  src: string;
  dst: string;
  kind: GraphEdgeKind;
  rel: string;
  count: number;
  firstLine: number;
  refName: string;
  provenance: "static" | "heuristic";
  confidence: number;
  evidence?: string;
};

export type ExploreCallPath = {
  from: string;
  to: string;
  nodes: string[];
};

export type ExploreImpactRef = {
  id: string;
  entity: StoredEntity | null;
};

export type ExploreBlastRadius = {
  rootId: string;
  dependents: ExploreImpactRef[];
  tests: ExploreImpactRef[];
};

export type ExploreChangeSurfaceRef = {
  rootId: string;
  id: string;
  rel: "type" | "return";
  entity: StoredEntity;
  rescued: boolean;
};

export type ExploreFileBundle = {
  file: FileInfo;
  score: number;
  isCentral: boolean;
  isChangeSurface: boolean;
  symbols: ExploreSymbolSnippet[];
  /** Zvec-layer assembled text for this file (entity content, clustered). */
  text: string;
};

export type ExploreSymbolSnippet = {
  id: string;
  name: string;
  kind?: string;
  range: Range;
  content: string;
};

export type ExploreResult = {
  available: boolean;
  query: string;
  roots: ExploreNode[];
  nodes: ExploreNode[];
  edges: ExploreEdge[];
  edgesTruncated: boolean;
  callPaths: ExploreCallPath[];
  blastRadius: ExploreBlastRadius[];
  changeSurface: ExploreChangeSurfaceRef[];
  dynamicBoundaries: DynamicBoundary[];
  dynamicBoundariesTruncated: boolean;
  files: ExploreFileBundle[];
  emptyReason?: "graph_unavailable" | "no_seeds" | "no_context";
};

export type ExploreSubgraphResult = {
  available: boolean;
  rootIds: string[];
  nodes: ExploreNode[];
  edges: ExploreEdge[];
  edgesTruncated: boolean;
  callPaths: ExploreCallPath[];
  nodeScores: ReadonlyMap<string, number>;
};
