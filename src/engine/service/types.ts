import type { EmbeddingModel } from "../models/index.js";
import type {
  CodeSymbolType,
  WorkspaceIndexPolicy,
  WorkspaceIndexStatus,
  WorkspaceIndexInfo,
  Content,
  EntityMetadata,
  IndexProgress,
  IndexResult,
  Range,
  RootPath,
  SearchPlanRoute,
  SearchHitTrace,
  SearchMatchedBy,
  TimingEntry,
} from "../types.js";

export type CreateZvecGrepOptions = {
  root?: string;
  home?: string;
  embeddingModel?: EmbeddingModel;
  embeddingModelOwnership?: "owned" | "borrowed";
  daemonInstanceToken?: string;
  embedding?: string;
  apiKey?: string;
  endpoint?: string;
  modelCacheDir?: string;
  device?: "auto" | "cpu" | "metal" | "vulkan" | "cuda";
  authorizationSigningKeyPath?: string;
};

export type ZvecGrepIndexOptions = {
  root?: string;
  rootPaths?: readonly (string | RootPath)[];
  rebuild?: boolean;
  resetPaths?: boolean;
  includePaths?: readonly string[];
  excludePaths?: readonly string[];
  globs?: readonly string[];
  insensitiveGlobs?: readonly string[];
  fileTypes?: readonly string[];
  excludedFileTypes?: readonly string[];
  hidden?: boolean;
  noIgnore?: boolean;
  ignoreFiles?: readonly string[];
  maxDepth?: number;
  maxFileSizeBytes?: number;
  follow?: boolean;
  embeddingConcurrency?: number;
  onProgress?: (progress: IndexProgress) => void;
  changedPaths?: readonly string[];
  signal?: AbortSignal;
  onWriterContext?: (
    context: ZvecGrepWriterContext,
  ) => void | (() => void | Promise<void>);
};

export type ZvecGrepWriterContext = (
  options: ZvecGrepContextOptions,
) => Promise<ZvecGrepContextResult>;

export type ZvecGrepInfoOptions = {
  root?: string;
  includeStatus?: boolean;
};

export type ZvecGrepInfoResult = {
  root: string;
  indexed: boolean;
  indexPolicy: WorkspaceIndexPolicy | "undecided";
  home: string;
  indexPath: string;
  source: "index" | "unindexed";
  workspaceIndex?: WorkspaceIndexInfo;
  status?: WorkspaceIndexStatus | null;
  suggestion?: string;
};

export type ZvecGrepContextRoute = SearchPlanRoute;

export type ZvecGrepContextOptions = {
  query?: string;
  queries?: readonly string[];
  rg?: boolean;
  rgOptions?: ZvecGrepSearchOptions;
  rgPaths?: readonly string[];
  routes?: readonly ZvecGrepContextRoute[];
  /** Fuse every query group into one ranked search plan. */
  fuse?: boolean;
  root?: string;
  limit?: number;
  autoUpdate?: boolean;
  onAutoUpdateProgress?: (progress: IndexProgress) => void;
  trace?: boolean;
  preferSymbol?: boolean;
  symbolTypes?: readonly CodeSymbolType[];
  includePaths?: readonly string[];
  excludePaths?: readonly string[];
  globs?: readonly string[];
  insensitiveGlobs?: readonly string[];
  fileTypes?: readonly string[];
  excludedFileTypes?: readonly string[];
  hidden?: boolean;
  noIgnore?: boolean;
  ignoreFiles?: readonly string[];
  maxDepth?: number;
  maxFileSizeBytes?: number;
  follow?: boolean;
  modifiedAfter?: number;
  modifiedBefore?: number;
  embeddingConcurrency?: number;
};

export type ZvecGrepExploreOptions = {
  root?: string;
  query: string;
  seedId?: string;
  searchLimit?: number;
  traversalDepth?: number;
  maxNodes?: number;
  maxFiles?: number;
  maxChars?: number;
};

export type ZvecGrepGraphNeighborhoodOptions = {
  root?: string;
  direction: ZvecGrepGraphDirection;
  query: string;
  seedId?: string;
  depth?: number;
  limit?: number;
};

export type ZvecGrepGraphDirection = "callers" | "callees" | "impact";

export type ZvecGrepGraphFile = {
  id: string;
  absolutePath: string;
  relativePath: string;
  rootPath?: string;
};

export type ZvecGrepGraphEntity = {
  entityId: string;
  name?: string;
  kind?: string;
  file: ZvecGrepGraphFile;
  range: Range;
};

export type ZvecGrepGraphNode = {
  id: string;
  kind?: string;
  isRoot: boolean;
  entity: ZvecGrepGraphEntity | null;
};

export type ZvecGrepGraphEdgeKind =
  | "CONTAINS"
  | "CALLS"
  | "REFS"
  | "INHERITS"
  | "DEFINES"
  | "IMPORTS"
  | "INSTANTIATES";

export type ZvecGrepGraphEdge = {
  src: string;
  dst: string;
  kind: ZvecGrepGraphEdgeKind;
  rel: string;
  count: number;
  firstLine: number;
  refName: string;
  provenance: "static" | "heuristic";
  confidence: number;
  evidence?: string;
};

export type ZvecGrepGraphSeed = {
  id: string;
  entity: ZvecGrepGraphEntity;
};

export type ZvecGrepGraphNeighbor = {
  id: string;
  kind?: string;
  count?: number;
  entity: ZvecGrepGraphEntity | null;
};

export type ZvecGrepExploreFileBundle = {
  file: ZvecGrepGraphFile;
  score: number;
  isCentral: boolean;
  isChangeSurface: boolean;
  symbols: {
    id: string;
    name: string;
    kind?: string;
    range: Range;
    content: string;
  }[];
  text: string;
};

export type ZvecGrepExploreResult = {
  root: string;
  available: boolean;
  unavailableReason?: string;
  query: string;
  roots: ZvecGrepGraphNode[];
  nodes: ZvecGrepGraphNode[];
  edges: ZvecGrepGraphEdge[];
  edgesTruncated: boolean;
  callPaths: { from: string; to: string; nodes: string[] }[];
  blastRadius: {
    rootId: string;
    dependents: { id: string; entity: ZvecGrepGraphEntity | null }[];
    tests: { id: string; entity: ZvecGrepGraphEntity | null }[];
  }[];
  changeSurface: {
    rootId: string;
    id: string;
    rel: "type" | "return";
    entity: ZvecGrepGraphEntity;
    rescued: boolean;
  }[];
  dynamicBoundaries: {
    sourceId: string;
    target: {
      raw: string;
      member: string;
      receiver?: { kind: "owner" | "super" | "qualified"; name: string };
      hints?: {
        receiverType?: string;
        candidateTypes?: string[];
        genericBounds?: string[];
        dispatch?: "static" | "virtual" | "interface" | "trait" | "dynamic";
        callArity?: number;
      };
    };
    reason: "unknown_receiver_type" | "polymorphic_dispatch";
    candidates: string[];
    candidatesTruncated: boolean;
    candidateDetails: {
      targetId: string;
      reason: "hierarchy" | "generic_bound" | "method_set";
      confidence: number;
    }[];
  }[];
  dynamicBoundariesTruncated: boolean;
  files: ZvecGrepExploreFileBundle[];
  emptyReason?: "graph_unavailable" | "no_seeds" | "no_context";
};

export type ZvecGrepGraphNeighborhoodResult = {
  root: string;
  available: boolean;
  unavailableReason?: string;
  direction: ZvecGrepGraphDirection;
  query: string;
  depth: number;
  limit: number;
  seeds: ZvecGrepGraphSeed[];
  ambiguous?: boolean;
  seed?: ZvecGrepGraphSeed;
  neighbors: ZvecGrepGraphNeighbor[];
};

export type ZvecGrepSearchOptions = {
  extraArgs?: readonly string[];
  patternFiles?: readonly string[];
  fixedStrings?: boolean;
  ignoreCase?: boolean;
  wordRegexp?: boolean;
  beforeContext?: number;
  afterContext?: number;
  hidden?: boolean;
};

export type ZvecGrepContextSource = "index" | "rg";

export type ZvecGrepContextCoverage =
  "ranked_sample" | "rg_exhaustive" | "rg_truncated";

export type ZvecGrepContextFile = {
  absolutePath: string;
  relativePath: string;
  rootPath?: string;
};

export type ZvecGrepContextItemKind = "indexed_entity" | "lexical_match";

export type ZvecGrepContextContainer = {
  entityId: string;
  range: Range;
  metadata?: EntityMetadata;
};

export type ZvecGrepContextQueryGroupMatch = {
  id: string;
  query: string;
  role: "primary" | "supplemental";
  rank: number;
  matchedBy: SearchMatchedBy;
};

export type ZvecGrepContextSelectionReason = "coverage" | "global_fill";

export type ZvecGrepContextItem = {
  kind: ZvecGrepContextItemKind;
  rank: number;
  file: ZvecGrepContextFile;
  range: Range;
  excerptRange?: Range;
  content: string;
  contentRole?: "source" | "outline";
  outline?: string;
  status: "fresh" | "possibly_stale";
  score?: number;
  matchedBy: SearchMatchedBy | "lexical";
  metadata?: EntityMetadata;
  entityId?: string;
  container?: ZvecGrepContextContainer;
  trace?: SearchHitTrace;
  queryGroups?: readonly ZvecGrepContextQueryGroupMatch[];
  selectionReason?: ZvecGrepContextSelectionReason;
  coverageGroup?: string;
};

export type ZvecGrepContextGroupResult = {
  id: string;
  query: string;
  role: "primary" | "supplemental";
  items: ZvecGrepContextItem[];
};

export type ZvecGrepContextWorkspaceIndex = {
  id: string;
  name: string;
  path: string;
};

export type ZvecGrepGraphRelationship = {
  srcId: string;
  dstId: string;
  srcLabel: string;
  dstLabel: string;
  kind: "CALLS" | "REFS" | "INHERITS" | "CONTAINS" | "IMPORTS" | "INSTANTIATES";
  scope: "symbol" | "file";
};

export type ZvecGrepRgDiagnostics = {
  backend: "bundled-rg" | "rg";
  command: string;
  args: readonly string[];
  ignoredDirectories: readonly string[];
  missingPaths?: readonly string[];
  searchedPaths?: readonly string[];
  limit?: number;
  truncated: boolean;
};

export type ZvecGrepStructureEnrichmentDiagnostics = {
  source: "structural_extraction";
  fileLimit: number;
  matchedFiles: number;
  parsedFiles: number;
  enrichedFiles: number;
  enrichedItems: number;
  skippedFiles: number;
  truncated: boolean;
};

export type ZvecGrepIndexDiagnostics = {
  hitsReturned: number;
  queryGroups?: readonly {
    id: string;
    query: string;
    role: "primary" | "supplemental";
  }[];
  routes: readonly {
    id: string;
    mode: "fts" | "vector";
    query: string;
  }[];
  graphExpand?: {
    available: boolean;
    unavailableReason?: string;
    seeds: number;
    neighborsAdded: number;
  };
};

export type ZvecGrepContextDiagnostics = {
  emptyReason?: "no_matches" | "no_searchable_files";
  index?: ZvecGrepIndexDiagnostics;
  rg?: ZvecGrepRgDiagnostics;
  structure?: ZvecGrepStructureEnrichmentDiagnostics;
  timings?: readonly TimingEntry[];
};

export type ZvecGrepContextResult = {
  query: string;
  root: string;
  source: ZvecGrepContextSource;
  coverage: ZvecGrepContextCoverage;
  workspaceIndex?: ZvecGrepContextWorkspaceIndex;
  items: ZvecGrepContextItem[];
  /** Per-query-group recall lists before cross-group deduplication and reranking. */
  groupResults?: ZvecGrepContextGroupResult[];
  relationships?: ZvecGrepGraphRelationship[];
  diagnostics: ZvecGrepContextDiagnostics;
};

export type ZvecGrep = {
  readonly root: string;
  index(options?: ZvecGrepIndexOptions): Promise<IndexResult>;
  dropIndex(options?: ZvecGrepInfoOptions): Promise<boolean>;
  disableIndex(options?: ZvecGrepInfoOptions): Promise<ZvecGrepInfoResult>;
  info(options?: ZvecGrepInfoOptions): Promise<ZvecGrepInfoResult>;
  context(options: ZvecGrepContextOptions): Promise<ZvecGrepContextResult>;
  explore(options: ZvecGrepExploreOptions): Promise<ZvecGrepExploreResult>;
  graphNeighborhood(
    options: ZvecGrepGraphNeighborhoodOptions,
  ): Promise<ZvecGrepGraphNeighborhoodResult>;
  close(): Promise<void>;
};

export type ZvecGrepContent = Content;
