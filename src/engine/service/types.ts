import type { EmbeddingModel } from "../models/index.js";
import type {
  PreparedWorkspaceContext,
  WorkspaceContextPlan,
  WorkspaceContextPreflight,
} from "./zvec-grep.js";
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
  SourceInvalidation,
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
  /** Force exact-path content checks and return a post-index committed proof. */
  verifySourcePaths?: readonly string[];
  signal?: AbortSignal;
  onWriterContext?: (
    context: ZvecGrepWriterContext,
    preflight: ZvecGrepWriterPreflight,
  ) => void | (() => void | Promise<void>);
};

export type ZvecGrepWriterContext = (
  options: ZvecGrepContextOptions,
  prepared?: PreparedWorkspaceContext,
) => Promise<ZvecGrepContextResult>;

export type ZvecGrepWriterPreflight = (
  plan: WorkspaceContextPlan,
) => Promise<WorkspaceContextPreflight>;

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
  matchedBy: SearchMatchedBy | "lexical" | "keyword";
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
  /** Failed byte checks for indexed sources actually returned by this search. */
  sourceInvalidations?: readonly SourceInvalidation[];
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
};

export type ZvecGrepContextDiagnostics = {
  emptyReason?: "no_matches" | "no_searchable_files" | "semantic_incomplete";
  /** Local results are usable, but omitted semantic recall is not no-match evidence. */
  semantic?:
    | {
        status: "skipped";
        reason: "preparation_budget_exceeded";
        budgetMs: number;
      }
    | {
        status: "skipped";
        reason: "index_unavailable";
      };
  index?: ZvecGrepIndexDiagnostics;
  rg?: ZvecGrepRgDiagnostics;
  /** Bounded, approximate current-source fallback, not literal/FTS evidence. */
  keywords?: {
    terms: readonly string[];
    candidates: number;
    truncated: boolean;
  };
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
  diagnostics: ZvecGrepContextDiagnostics;
};

export type ZvecGrep = {
  readonly root: string;
  index(options?: ZvecGrepIndexOptions): Promise<IndexResult>;
  dropIndex(options?: ZvecGrepInfoOptions): Promise<boolean>;
  disableIndex(options?: ZvecGrepInfoOptions): Promise<ZvecGrepInfoResult>;
  info(options?: ZvecGrepInfoOptions): Promise<ZvecGrepInfoResult>;
  context(options: ZvecGrepContextOptions): Promise<ZvecGrepContextResult>;
  close(): Promise<void>;
};

export type ZvecGrepContent = Content;
