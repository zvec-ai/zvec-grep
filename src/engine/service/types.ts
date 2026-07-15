import type { EmbeddingModel } from "../models/embeddings.js";
import type {
  CodeSymbolType,
  CollectionIndexPolicy,
  CollectionIndexStatus,
  CollectionInfo,
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
  embedding?: string;
  apiKey?: string;
  endpoint?: string;
  modelCacheDir?: string;
  llamaGpu?: "auto" | "metal" | "vulkan" | "cuda" | false;
  embeddingParallelism?: number;
  defaultEmbedding?: boolean;
};

export type ZvecGrepIndexOptions = {
  root?: string;
  rootPaths?: readonly (string | RootPath)[];
  rebuild?: boolean;
  resetPaths?: boolean;
  includePaths?: readonly string[];
  excludePaths?: readonly string[];
  embeddingConcurrency?: number;
  onProgress?: (progress: IndexProgress) => void;
};

export type ZvecGrepInfoOptions = {
  root?: string;
};

export type ZvecGrepInfoResult = {
  root: string;
  indexed: boolean;
  indexPolicy: CollectionIndexPolicy | "undecided";
  home: string;
  indexPath: string;
  source: "index" | "unindexed";
  collection?: CollectionInfo;
  status?: CollectionIndexStatus | null;
  suggestion?: string;
};

export type ZvecGrepContextRoute = SearchPlanRoute;

export type ZvecGrepFallbackPolicy = "auto" | "disabled";

export type ZvecGrepContextOptions = {
  query?: string;
  queries?: readonly string[];
  rg?: boolean;
  rgOptions?: ZvecGrepSearchOptions;
  rgPaths?: readonly string[];
  routes?: readonly ZvecGrepContextRoute[];
  root?: string;
  collection?: string;
  limit?: number;
  /** @deprecated Anonymous context queries require an index; use rg for explicit no-index lexical search. */
  fallback?: ZvecGrepFallbackPolicy;
  autoUpdate?: boolean;
  onAutoUpdateProgress?: (progress: IndexProgress) => void;
  trace?: boolean;
  preferSymbol?: boolean;
  symbolTypes?: readonly CodeSymbolType[];
  includePaths?: readonly string[];
  excludePaths?: readonly string[];
  modifiedAfter?: number;
  modifiedBefore?: number;
  embeddingConcurrency?: number;
};

export type ZvecGrepSearchOptions = {
  extraArgs?: readonly string[];
  fixedStrings?: boolean;
  ignoreCase?: boolean;
  wordRegexp?: boolean;
  beforeContext?: number;
  afterContext?: number;
  hidden?: boolean;
};

export type ZvecGrepContextSource = "index" | "rg" | "lexical_fallback";

export type ZvecGrepContextCoverage =
  "ranked_sample" | "lexical_exhaustive" | "lexical_truncated";

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
};

export type ZvecGrepContextCollection = {
  id: string;
  name: string;
  path: string;
  anonymous: boolean;
};

export type ZvecGrepLexicalFallbackDiagnostics = {
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
  routes: readonly {
    id: string;
    mode: "fts" | "vector";
    query: string;
  }[];
};

export type ZvecGrepContextDiagnostics = {
  emptyReason?:
    | "no_matches"
    | "no_searchable_files"
    | "index_unavailable"
    | "search_failed";
  index?: ZvecGrepIndexDiagnostics;
  fallback?: ZvecGrepLexicalFallbackDiagnostics;
  structure?: ZvecGrepStructureEnrichmentDiagnostics;
  timings?: readonly TimingEntry[];
};

export type ZvecGrepContextResult = {
  query: string;
  root: string;
  source: ZvecGrepContextSource;
  coverage: ZvecGrepContextCoverage;
  collection?: ZvecGrepContextCollection;
  items: ZvecGrepContextItem[];
  diagnostics: ZvecGrepContextDiagnostics;
};

export type ZvecGrepCollectionIndexOptions = {
  rebuild?: boolean;
  resetPaths?: boolean;
  includePaths?: readonly string[];
  excludePaths?: readonly string[];
  embeddingConcurrency?: number;
  onProgress?: (progress: IndexProgress) => void;
};

export type ZvecGrepCollections = {
  list(): Promise<CollectionInfo[]>;
  info(name: string): Promise<CollectionInfo | null>;
  status(name: string): Promise<CollectionIndexStatus | null>;
  index(
    name: string,
    paths?: string | RootPath | readonly (string | RootPath)[],
    options?: ZvecGrepCollectionIndexOptions,
  ): Promise<IndexResult>;
  remove(name: string): Promise<boolean>;
};

export type ZvecGrep = {
  readonly root: string;
  readonly collections: ZvecGrepCollections;
  index(options?: ZvecGrepIndexOptions): Promise<IndexResult>;
  disableIndex(options?: ZvecGrepInfoOptions): Promise<ZvecGrepInfoResult>;
  info(options?: ZvecGrepInfoOptions): Promise<ZvecGrepInfoResult>;
  context(options: ZvecGrepContextOptions): Promise<ZvecGrepContextResult>;
  close(): Promise<void>;
};

export type ZvecGrepContent = Content;
