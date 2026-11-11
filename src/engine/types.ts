// -----------------------------------------------------------------------------
// Content types
// -----------------------------------------------------------------------------

export type TextContent = {
  kind: "text";
  text: string;
};

export type ImageFormat = "png" | "jpeg" | "webp" | "gif";

export type ImageContent = {
  kind: "image";
  data: Uint8Array;
  format: ImageFormat;
};

export type Content = TextContent | ImageContent;

export type ContentKind = Content["kind"];

export function isTextContent(content: Content): content is TextContent {
  return content.kind === "text";
}

export function isImageContent(content: Content): content is ImageContent {
  return content.kind === "image";
}

// -----------------------------------------------------------------------------
// File types
// -----------------------------------------------------------------------------

export type FileKind = "text" | "code" | "data" | "image";

export type FileFormat = string;

export type FileIndexStatus = {
  indexedTime: number | null;
  entityCount: number;
  tokenCount?: number;
  truncatedFragmentCount?: number;
  error?: string;
};

export type RootPath = {
  absolutePath: string;
  recursive: boolean;
  include?: readonly string[];
  exclude?: readonly string[];
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
};

export type FileInfo = {
  id: string;
  absolutePath: string;
  relativePath: string;
  rootPath: string;
  sizeBytes: number;
  lastModifiedTime: number;
  contentHash?: string;
  kind: FileKind;
  format: FileFormat;
  indexStatus?: FileIndexStatus;
};

export type SkippedFileReason =
  "empty" | "too_large" | "unsupported" | "binary";

export type SkippedFile = {
  absolutePath: string;
  relativePath: string;
  reason: SkippedFileReason;
  sizeBytes?: number;
  limitBytes?: number;
};

export type FileScanDiagnostics = {
  skippedFiles: number;
  skippedByReason: Record<SkippedFileReason, number>;
  skippedSamples: SkippedFile[];
};

// -----------------------------------------------------------------------------
// Entity and fragment types
// -----------------------------------------------------------------------------

export type FileRange = {
  kind: "file";
};

export type TextRange = {
  kind: "text";
  startLine: number;
  endLine: number;
  startOffset: number;
  endOffset: number;
};

export type ByteRange = {
  kind: "byte";
  startOffset: number;
  endOffset: number;
};

export type PageRange = {
  kind: "page";
  page: number;
};

export type PageTextRange = {
  kind: "page_text";
  page: number;
  startOffset: number;
  endOffset: number;
};

export type PageRegionRange = {
  kind: "page_region";
  page: number;
  x: number;
  y: number;
  width: number;
  height: number;
};

export type Range =
  | FileRange
  | TextRange
  | ByteRange
  | PageRange
  | PageTextRange
  | PageRegionRange;

export type CodeSymbolType =
  "module" | "class" | "interface" | "function" | "value" | "alias";

export type CodeEntityModifier =
  | "exported"
  | "async"
  | "static"
  | "public"
  | "private"
  | "protected"
  | "internal";

export type CodeEntityMetadata = {
  kind: "code";
  symbolType: CodeSymbolType;
  symbolName: string | null;
  scope: string | null;
  nodeType: string | null;
  signature: string | null;
  doc: string | null;
  modifiers: readonly CodeEntityModifier[];
};

export type MarkdownEntityMetadata = {
  kind: "markdown";
  heading: string | null;
  level: number | null;
  scope: string | null;
};

export type EntityMetadata = CodeEntityMetadata | MarkdownEntityMetadata;

export type Entity = {
  id: string;
  fileId: string;
  range: Range;
  content: Content;
  metadata?: EntityMetadata;
};

export type EntityFragment = {
  id: string;
  group?: string;
  fileId: string;
  range: Range;
  content: Content;
  metadata?: EntityMetadata;
};

// -----------------------------------------------------------------------------
// Workspace index types
// -----------------------------------------------------------------------------

export const CURRENT_INDEX_VERSION = 1;

export type WorkspaceIndexEmbeddingSchema = {
  provider: string;
  model: string;
  dimension: number;
  metric: string;
};

export type WorkspaceIndexPolicy = "enabled" | "disabled";

export type WorkspaceIndexInfo = {
  id: string;
  name: string;
  path: string;
  rootPaths: readonly RootPath[];
  indexPolicy?: WorkspaceIndexPolicy;
  embedding?: WorkspaceIndexEmbeddingSchema | null;
  indexVersion?: number | null;
  createdTime: number;
  updatedTime: number;
};

// -----------------------------------------------------------------------------
// Indexing types
// -----------------------------------------------------------------------------

export type IndexProgressPhase = "scanning" | "indexing" | "done";

export type IndexEmbeddingProgress = {
  concurrency?: number;
  maxConcurrency?: number;
  retryableFailures?: number;
  stage?: "preparing" | "downloading" | "ready" | "warning";
  model?: string;
  downloadedBytes?: number;
  totalBytes?: number;
  message?: string;
};

export type IndexProgress = {
  phase: IndexProgressPhase;
  filesTotal?: number;
  filesIndexed?: number;
  filesFailed?: number;
  detail?: string;
  embedding?: IndexEmbeddingProgress;
};

export type IndexOptions = {
  name?: string;
  rebuild?: boolean;
  embeddingConcurrency?: number;
  onProgress?: (progress: IndexProgress) => void;
  changedPaths?: readonly string[];
  signal?: AbortSignal;
};

export type TimingEntry = {
  name: string;
  durationMs: number;
  count?: number;
};

export type IndexResult = {
  filesScanned: number;
  filesAdded: number;
  filesModified: number;
  filesPending: number;
  filesDeleted: number;
  filesUnchanged: number;
  filesFailed: number;
  entitiesCreated: number;
  durationMs: number;
  timings?: readonly TimingEntry[];
  scanDiagnostics?: FileScanDiagnostics;
};

export type WorkspaceIndexStatus = {
  filesScanned: number;
  filesStored: number;
  filesIndexed: number;
  entitiesIndexed: number;
  fragmentsTruncated: number;
  filesPending: number;
  filesFailed: number;
  filesAdded: number;
  filesModified: number;
  filesDeleted: number;
  filesUnchanged: number;
  pendingFiles: FileInfo[];
  failedFiles: FileInfo[];
  addedFiles: FileInfo[];
  modifiedFiles: FileInfo[];
  deletedFiles: FileInfo[];
};

// -----------------------------------------------------------------------------
// Search types
// -----------------------------------------------------------------------------

export type SearchMatchedBy = "fts" | "vector" | "fts+vector";

export const SEMANTIC_FEATURES_VERSION = 1 as const;

export type SemanticRepresentation = "symbol" | "body" | "document" | "all";
export type SemanticTokenSketch = "off" | "trace" | "rerank";
export type SemanticGraphRerank = "off" | "shadow" | "on";
export type SemanticGraphEdgeConfidence = "syntax" | "strong";
export type SemanticGraphExpansion = "off" | "related";
export type SemanticTraceMode = "off" | "summary" | "full";

export type SemanticFeatures = {
  semantic_features_version: typeof SEMANTIC_FEATURES_VERSION;
  representation: SemanticRepresentation;
  token_sketch: SemanticTokenSketch;
  graph_rerank: SemanticGraphRerank;
  graph_edge_confidence: SemanticGraphEdgeConfidence;
  graph_expansion: SemanticGraphExpansion;
  trace: SemanticTraceMode;
};

export const DEFAULT_SEMANTIC_FEATURES: SemanticFeatures = {
  semantic_features_version: SEMANTIC_FEATURES_VERSION,
  representation: "all",
  token_sketch: "off",
  graph_rerank: "off",
  graph_edge_confidence: "strong",
  graph_expansion: "off",
  trace: "off",
};

export type SearchTraceFeatures = {
  nameOverlap: number;
  tokenSketch: number;
  graph: number;
};

export type SearchTraceContributions = {
  baseHybrid: number;
  nameOverlap: number;
  tokenSketch: number;
  graph: number;
};

export type SearchTraceGraphEvidence = {
  edge: string;
  source: string;
  target: string;
  resolution: string;
  producer: string;
  producer_context_sha256: string;
  confidence: "definite" | "syntax";
};

export type SemanticSearchTraceCandidate = {
  entity_id: string;
  fts_rank?: number;
  vector_rank?: number;
  representation: SemanticRepresentation;
  features: {
    name_overlap: number;
    token_sketch: number;
    graph: number;
  };
  contributions: {
    base_hybrid: number;
    name_overlap: number;
    token_sketch: number;
    graph: number;
  };
  final_score: number;
  graph_evidence: SearchTraceGraphEvidence[];
};

export type SemanticSearchTrace = {
  schema_version: 1;
  generation_id: string;
  source_digest: string;
  source_selection_digest: string;
  source_files: number;
  source_bytes: number;
  feature_config: SemanticFeatures;
  artifacts: {
    index_sha256: string;
    graph_generation_id: string | null;
    graph_artifact_sha256: string | null;
    graph_publication_sha256: string | null;
    embedding_model: string;
    embedding_schema: string;
  };
  candidates: SemanticSearchTraceCandidate[];
  candidate_count: number;
  candidates_truncated: boolean;
  graph: {
    binding: "bound" | "unbound";
    generation_id: string | null;
    artifact_sha256: string | null;
    publication_sha256: string | null;
    seed_ids: string[];
    eligible_edges: number;
    excluded_edges: {
      unresolved: number;
      ambiguous: number;
      stale: number;
    };
  };
  expansion: {
    enabled: boolean;
    related_count: number;
    items: [];
  };
};

export type SearchRecallTrace = {
  path: "fts" | "vector";
  routeId?: string;
  query?: string;
  found: boolean;
  forced?: boolean;
  rank?: number;
  score?: number;
  reason?: string;
};

export type SearchStageTrace = {
  rank: number;
  score: number;
  forced?: boolean;
};

export type SearchFinalTrace = {
  returnedByLimit: boolean;
  cutoffRank: number;
};

export type SearchHitTrace = {
  recall: SearchRecallTrace[];
  fusion?: SearchStageTrace;
  ranking?: SearchStageTrace;
  final: SearchFinalTrace;
  representation?: SemanticRepresentation;
  features?: SearchTraceFeatures;
  contributions?: SearchTraceContributions;
  finalScore?: number;
};

export type SearchHitEvidence = {
  range: Range;
  content: Content;
  metadata?: EntityMetadata;
  isEntity: boolean;
  path: "fts" | "vector";
  routeId?: string;
  query?: string;
  rank?: number;
  score?: number;
  forced?: boolean;
};

export type SearchHit = {
  entity: Entity;
  file: FileInfo;
  evidence: SearchHitEvidence[];
  rank: number;
  score: number;
  matchedBy: SearchMatchedBy;
  trace?: SearchHitTrace;
};

export type SearchPlanRouteMode = "fts" | "vector";

export type SearchPlanRoute = {
  mode: SearchPlanRouteMode;
  query: string;
};

export type ResolvedSearchPlanRoute = {
  id: string;
  mode: SearchPlanRouteMode;
  query: string;
};

export type SearchPlan = {
  routes: readonly SearchPlanRoute[];
  limit?: number;
  trace?: boolean;
  semanticFeatures?: SemanticFeatures;
  trackEntityId?: string;
  preferSymbol?: boolean;
  symbolTypes?: readonly CodeSymbolType[];
  includePaths?: readonly string[];
  excludePaths?: readonly string[];
  globs?: readonly string[];
  insensitiveGlobs?: readonly string[];
  fileTypes?: readonly string[];
  excludedFileTypes?: readonly string[];
  modifiedAfter?: number;
  modifiedBefore?: number;
};

export type ResolvedSearchPlan = Omit<SearchPlan, "routes"> & {
  routes: readonly ResolvedSearchPlanRoute[];
};

export type SearchPlanResult = {
  plan: ResolvedSearchPlan;
  hits: SearchHit[];
  trackedHit?: SearchHit;
  trace?: SemanticSearchTrace;
  timings?: readonly TimingEntry[];
};

// -----------------------------------------------------------------------------
// Diagnostics
// -----------------------------------------------------------------------------

export type EntitySearchDiagnosis = {
  query: string;
  entityId: string;
  file: FileInfo;
  entity: Entity;
  search: SearchPlanResult;
};
