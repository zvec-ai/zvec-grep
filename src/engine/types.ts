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
  error?: string;
};

export type RootPath = {
  absolutePath: string;
  recursive: boolean;
  include?: readonly string[];
  exclude?: readonly string[];
};

export type FileInfo = {
  id: string;
  collectionId: string;
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
// Collection types
// -----------------------------------------------------------------------------

export const CURRENT_INDEX_VERSION = 1;

export type CollectionEmbeddingSchema = {
  provider: string;
  model: string;
  dimension: number;
  metric: string;
};

export type CollectionIndexPolicy = "enabled" | "disabled";

export type CollectionInfo = {
  id: string;
  name: string;
  path: string;
  rootPaths: readonly RootPath[];
  indexPolicy?: CollectionIndexPolicy;
  embedding?: CollectionEmbeddingSchema | null;
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
};

export type TimingEntry = {
  name: string;
  durationMs: number;
  count?: number;
};

export type IndexResult = {
  collectionId: string;
  collectionName: string;
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
};

export type CollectionIndexStatus = {
  collectionId: string;
  collectionName: string;
  filesScanned: number;
  filesStored: number;
  filesIndexed: number;
  entitiesIndexed: number;
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
  trackEntityId?: string;
  preferSymbol?: boolean;
  symbolTypes?: readonly CodeSymbolType[];
  includePaths?: readonly string[];
  excludePaths?: readonly string[];
  modifiedAfter?: number;
  modifiedBefore?: number;
};

export type ResolvedSearchPlan = Omit<SearchPlan, "routes"> & {
  routes: readonly ResolvedSearchPlanRoute[];
};

export type SearchPlanResult = {
  collectionId: string;
  collectionName: string;
  plan: ResolvedSearchPlan;
  hits: SearchHit[];
  trackedHit?: SearchHit;
  timings?: readonly TimingEntry[];
};

// -----------------------------------------------------------------------------
// Diagnostics
// -----------------------------------------------------------------------------

export type FileDiagnosis = {
  collectionId: string;
  collectionName: string;
  absolutePath: string;
  belongsToCollection: boolean;
  matchedRootPath?: string;
  file?: FileInfo;
  entityCount: number;
  reason?: string;
};

export type EntitySearchDiagnosis = {
  query: string;
  entityId: string;
  file: FileInfo;
  entity: Entity;
  search: SearchPlanResult;
};
