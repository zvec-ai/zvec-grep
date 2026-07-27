import { isAbsolute } from "node:path";
import { z } from "zod";

export const MCP_MAX_QUERY_GROUPS = 32;
export const MCP_MAX_QUERY_CHARS = 4_000;
export const MCP_MAX_PATH_FILTERS = 128;
export const MCP_MAX_PATH_CHARS = 1_024;
export const MCP_MAX_SEARCH_LIMIT = 50;

const boundedString = (description: string) =>
  z.string().max(MCP_MAX_QUERY_CHARS).describe(description);

const boundedStringList = (description: string) =>
  z
    .union([
      boundedString(description),
      z.array(boundedString(description)).max(MCP_MAX_QUERY_GROUPS),
    ])
    .optional();

const pathFilter = z.string().max(MCP_MAX_PATH_CHARS);
const legacyStringListInputSchema = z
  .union([z.string(), z.array(z.string())])
  .optional();
const legacyTimeInputSchema = z
  .union([z.number().int().nonnegative(), z.string()])
  .optional();

export const stringListInputSchema = boundedStringList(
  "A string or a list of strings.",
);
export const pathFilterInputSchema = z
  .union([pathFilter, z.array(pathFilter).max(MCP_MAX_PATH_FILTERS)])
  .optional();
export const timeInputSchema = z
  .union([z.number().int().nonnegative(), z.string().max(128)])
  .optional();
export const codeSymbolTypeSchema = z.enum([
  "module",
  "class",
  "interface",
  "function",
  "value",
  "alias",
]);

const fileTypesDescription =
  "Ripgrep file type names from rg --type-list to include, such as ts, py, h, or cpp.";
const excludedFileTypesDescription =
  "Ripgrep file type names from rg --type-list to exclude.";

export const absoluteRootSchema = z
  .string()
  .trim()
  .min(1, "root is required.")
  .max(MCP_MAX_PATH_CHARS)
  .refine((root) => isAbsolute(root), "root must be an absolute path.")
  .describe("Absolute repository path visible to the daemon.");

const searchFields = {
  query: boundedString("Natural-language or exact search query.").optional(),
  queries: boundedStringList(
    "Multiple query groups. Use this for related concepts.",
  ),
  fts: boundedStringList(
    "Exact lexical anchors, such as symbols, flags, or error messages.",
  ),
  vector: boundedStringList("Explicit semantic/vector-only queries."),
  limit: z
    .number()
    .int()
    .positive()
    .max(MCP_MAX_SEARCH_LIMIT)
    .optional()
    .describe("Maximum returned items per query/group."),
  include: pathFilterInputSchema.describe("Glob filters for paths to include."),
  exclude: pathFilterInputSchema.describe("Glob filters for paths to exclude."),
  globs: pathFilterInputSchema.describe(
    "Ordered case-sensitive rg-style glob rules. Later rules override earlier rules.",
  ),
  insensitiveGlobs: pathFilterInputSchema.describe(
    "Ordered case-insensitive rg-style glob rules. Later rules override earlier rules.",
  ),
  fileTypes: pathFilterInputSchema.describe(fileTypesDescription),
  excludedFileTypes: pathFilterInputSchema.describe(
    excludedFileTypesDescription,
  ),
  hidden: z.boolean().optional().describe("Include hidden paths."),
  noIgnore: z.boolean().optional().describe("Ignore no ignore files."),
  ignoreFiles: pathFilterInputSchema.describe(
    "Additional ignore files relative to the repository root.",
  ),
  maxDepth: z
    .number()
    .int()
    .nonnegative()
    .optional()
    .describe("Maximum recursive directory depth."),
  maxFileSizeBytes: z
    .number()
    .int()
    .positive()
    .optional()
    .describe("Maximum indexed file size in bytes."),
  follow: z.boolean().optional().describe("Follow symbolic links."),
  embeddingConcurrency: z
    .number()
    .int()
    .positive()
    .optional()
    .describe("Embedding requests processed concurrently during updates."),
  fuse: z
    .boolean()
    .optional()
    .describe(
      "Fuse every query group into one ranked search plan. Defaults to true; set false to preserve independent groups.",
    ),
  preferSymbol: z
    .boolean()
    .optional()
    .describe("Prefer exact indexed symbols when query names a symbol."),
  symbolTypes: z
    .array(codeSymbolTypeSchema)
    .max(6)
    .default([])
    .describe("Restrict indexed results to symbol types."),
  modifiedAfter: timeInputSchema.describe(
    "Only query files modified after this time.",
  ),
  modifiedBefore: timeInputSchema.describe(
    "Only query files modified before this time.",
  ),
  trace: z
    .boolean()
    .optional()
    .describe("Include per-hit search trace in structured output."),
};

export const zvecGrepIndexInputSchema = z.object({
  root: absoluteRootSchema,
  drop: z
    .boolean()
    .optional()
    .describe(
      "Permanently remove the workspace index. Use only when index deletion is explicitly requested, and do not combine with indexing options.",
    ),
  embedding: z
    .string()
    .trim()
    .min(1)
    .max(256)
    .optional()
    .describe(
      "Embedding model reference for a new index. Use a user-selected embedding, or omit only when a server default model is known; never guess a model.",
    ),
  rebuild: z
    .boolean()
    .optional()
    .describe(
      "Explicitly rebuild the existing index. Use only when rebuild was requested or required by an incompatible schema or index version.",
    ),
  resetPaths: z
    .boolean()
    .optional()
    .describe("Replace the index root-path configuration."),
  globs: pathFilterInputSchema.describe(
    "Ordered case-sensitive rg-style glob rules for indexed files.",
  ),
  insensitiveGlobs: pathFilterInputSchema.describe(
    "Ordered case-insensitive rg-style glob rules for indexed files.",
  ),
  fileTypes: pathFilterInputSchema.describe(fileTypesDescription),
  excludedFileTypes: pathFilterInputSchema.describe(
    excludedFileTypesDescription,
  ),
  hidden: z.boolean().optional().describe("Include hidden paths."),
  noIgnore: z.boolean().optional().describe("Ignore no ignore files."),
  ignoreFiles: pathFilterInputSchema.describe(
    "Additional ignore files relative to the repository root.",
  ),
  maxDepth: z
    .number()
    .int()
    .nonnegative()
    .optional()
    .describe("Maximum recursive directory depth."),
  maxFileSizeBytes: z
    .number()
    .int()
    .positive()
    .optional()
    .describe("Maximum indexed file size in bytes."),
  follow: z.boolean().optional().describe("Follow symbolic links."),
  embeddingConcurrency: z
    .number()
    .int()
    .positive()
    .optional()
    .describe("Embedding requests processed concurrently."),
  wait: z
    .boolean()
    .optional()
    .describe(
      "Wait for the submitted index job to finish. Defaults to false: submit in the background and poll zvec_grep_index_status only when completion, progress monitoring, or diagnostics are required.",
    ),
});

export const zvecGrepSearchInputSchema = z.object({
  root: absoluteRootSchema,
  ...searchFields,
  freshness: z
    .enum(["eventual", "wait_for_fresh"])
    .default("eventual")
    .describe(
      "Whether to search immediately or wait for the active index to become fresh.",
    ),
  autoUpdate: z
    .boolean()
    .default(true)
    .describe(
      "Whether an eventual search may schedule a background index update.",
    ),
});

export const zvecGrepIndexStatusInputSchema = z.object({
  root: absoluteRootSchema,
});

export const zvecGrepIndexDropInputSchema = z.object({
  root: absoluteRootSchema,
});

export const zvecGrepServerStatusInputSchema = z.object({});

export const zvecGrepRgInputSchema = z.object({
  pattern: z
    .string()
    .optional()
    .describe(
      "Regex or literal pattern to search for. Use zvec_grep_rg for exhaustive lexical search, unindexed repositories that can be answered lexically, or explicit rg-mode requests.",
    ),
  patterns: legacyStringListInputSchema.describe(
    "Multiple regex or literal patterns to search for.",
  ),
  root: absoluteRootSchema,
  paths: legacyStringListInputSchema.describe(
    "Optional paths to search within the root.",
  ),
  fixedStrings: z
    .boolean()
    .optional()
    .describe("Treat pattern as a literal string."),
  ignoreCase: z.boolean().optional().describe("Search case-insensitively."),
  wordRegexp: z.boolean().optional().describe("Only match whole words."),
  context: z
    .number()
    .int()
    .nonnegative()
    .max(20)
    .optional()
    .describe("Context lines before and after each match."),
  beforeContext: z
    .number()
    .int()
    .nonnegative()
    .max(20)
    .optional()
    .describe("Context lines before each match."),
  afterContext: z
    .number()
    .int()
    .nonnegative()
    .max(20)
    .optional()
    .describe("Context lines after each match."),
  hidden: z
    .boolean()
    .optional()
    .describe("Search hidden files and directories."),
  glob: legacyStringListInputSchema.describe(
    "ripgrep glob filters, for example '*.ts' or '!dist/**'. Accepts an array or comma-separated string.",
  ),
  limit: z
    .number()
    .int()
    .positive()
    .max(200)
    .optional()
    .describe("Maximum returned matches."),
});

const jobStateSchema = z.enum([
  "queued",
  "running",
  "succeeded",
  "failed",
  "cancelled",
]);
const jobErrorSchema = z.object({
  code: z.string(),
  message: z.string(),
});
const rangeSchema = z.union([
  z.object({ kind: z.literal("file") }),
  z.object({
    kind: z.literal("text"),
    startLine: z.number().int().nonnegative(),
    endLine: z.number().int().nonnegative(),
    startOffset: z.number().int().nonnegative(),
    endOffset: z.number().int().nonnegative(),
  }),
  z.object({
    kind: z.literal("byte"),
    startOffset: z.number().int().nonnegative(),
    endOffset: z.number().int().nonnegative(),
  }),
  z.object({ kind: z.literal("page"), page: z.number().int().nonnegative() }),
  z.object({
    kind: z.literal("page_text"),
    page: z.number().int().nonnegative(),
    startOffset: z.number().int().nonnegative(),
    endOffset: z.number().int().nonnegative(),
  }),
  z.object({
    kind: z.literal("page_region"),
    page: z.number().int().nonnegative(),
    x: z.number(),
    y: z.number(),
    width: z.number().nonnegative(),
    height: z.number().nonnegative(),
  }),
]);
const contextFileSchema = z.object({
  absolutePath: z.string(),
  relativePath: z.string(),
  rootPath: z.string().optional(),
});
const contextItemSchema = z.object({
  kind: z.enum(["indexed_entity", "lexical_match"]),
  rank: z.number().int().positive(),
  file: contextFileSchema,
  range: rangeSchema,
  excerptRange: rangeSchema.optional(),
  outline: z.string().optional(),
  content: z.string(),
  contentRole: z.enum(["source", "outline"]).optional(),
  status: z.enum(["fresh", "possibly_stale"]),
  score: z.number().optional(),
  matchedBy: z.enum(["fts", "vector", "fts+vector", "lexical"]),
  metadata: z.unknown().optional(),
  entityId: z.string().optional(),
  container: z
    .object({
      entityId: z.string(),
      range: rangeSchema,
      metadata: z.unknown().optional(),
    })
    .optional(),
  trace: z.unknown().optional(),
});
const searchResultSchema = z.object({
  query: z.string(),
  root: z.string(),
  source: z.enum(["index", "rg", "lexical_fallback"]),
  coverage: z.enum([
    "ranked_sample",
    "rg_exhaustive",
    "rg_truncated",
    "lexical_exhaustive",
    "lexical_truncated",
  ]),
  collection: z
    .object({
      id: z.string(),
      name: z.string(),
      path: z.string(),
      anonymous: z.boolean(),
    })
    .optional(),
  diagnostics: z.object({
    emptyReason: z
      .enum([
        "no_matches",
        "no_searchable_files",
        "index_unavailable",
        "search_failed",
      ])
      .optional(),
    index: z.unknown().optional(),
    fallback: z.unknown().optional(),
    rg: z.unknown().optional(),
    structure: z.unknown().optional(),
    timings: z
      .array(
        z.object({
          name: z.string(),
          durationMs: z.number().nonnegative(),
          count: z.number().int().nonnegative().optional(),
        }),
      )
      .optional(),
  }),
  items: z.array(contextItemSchema),
});

export const zvecGrepIndexOutputSchema = z.object({
  root: z.string(),
  job_id: z.string(),
  state: jobStateSchema,
  reused: z.boolean(),
  action: z.enum(["index", "drop"]).optional(),
  dropped: z.boolean().optional(),
  error: jobErrorSchema.optional(),
});

export const zvecGrepIndexDropOutputSchema = z.object({
  root: z.string(),
  removed: z.boolean(),
});

export const zvecGrepSearchIndexingSchema = z
  .object({
    state: z
      .enum(["idle", "queued", "running", "failed", "cancelled"])
      .describe("Current background indexing state."),
    completed: z
      .number()
      .int()
      .nonnegative()
      .optional()
      .describe("Up-to-date indexed files in the configured file scope."),
    total: z
      .number()
      .int()
      .nonnegative()
      .optional()
      .describe("Total files in the configured file scope."),
  })
  .describe("Compact indexing snapshot included with possibly stale results.");

export type ZvecGrepSearchIndexing = z.infer<
  typeof zvecGrepSearchIndexingSchema
>;

export const zvecGrepSearchOutputSchema = z.object({
  root: z.string(),
  freshness: z.enum(["fresh", "possibly_stale"]),
  indexing: zvecGrepSearchIndexingSchema.optional(),
  result: searchResultSchema,
});

export const zvecGrepIndexStatusOutputSchema = z.object({
  root: z.string(),
  indexed: z.boolean(),
  index_policy: z.enum(["enabled", "disabled", "undecided"]),
  source: z.enum(["index", "unindexed"]),
  persistent: z.object({
    home: z.string(),
    index_path: z.string(),
    collection: z
      .object({
        id: z.string(),
        name: z.string(),
        path: z.string(),
        root_paths: z.array(
          z.object({
            absolute_path: z.string(),
            recursive: z.boolean(),
            include: z.array(z.string()).optional(),
            exclude: z.array(z.string()).optional(),
            globs: z.array(z.string()).optional(),
            insensitive_globs: z.array(z.string()).optional(),
            file_types: z.array(z.string()).optional(),
            excluded_file_types: z.array(z.string()).optional(),
            hidden: z.boolean().optional(),
            no_ignore: z.boolean().optional(),
            ignore_files: z.array(z.string()).optional(),
            max_depth: z.number().int().nonnegative().optional(),
            max_file_size_bytes: z.number().int().positive().optional(),
            follow: z.boolean().optional(),
          }),
        ),
        embedding: z
          .object({
            provider: z.string(),
            model: z.string(),
            dimension: z.number().int().positive(),
            metric: z.string(),
          })
          .nullable()
          .optional(),
        index_version: z.number().int().nonnegative().nullable().optional(),
        created_time: z.number().nonnegative(),
        updated_time: z.number().nonnegative(),
      })
      .optional(),
    files: z
      .object({
        stored: z.number().int().nonnegative(),
        scanned: z.number().int().nonnegative().optional(),
        indexed: z.number().int().nonnegative(),
        pending: z.number().int().nonnegative(),
        failed: z.number().int().nonnegative(),
        added: z.number().int().nonnegative(),
        modified: z.number().int().nonnegative(),
        deleted: z.number().int().nonnegative(),
        unchanged: z.number().int().nonnegative(),
        entities: z.number().int().nonnegative(),
        truncated_fragments: z.number().int().nonnegative(),
      })
      .optional(),
    suggestion: z.string().optional(),
  }),
  runtime: z
    .object({
      watcher_active: z.boolean(),
      dirty_revision: z.number().int().nonnegative(),
      indexed_revision: z.number().int().nonnegative(),
      active_job_id: z.string().optional(),
      job_state: jobStateSchema.optional(),
      progress: z
        .object({
          phase: z.enum(["scanning", "indexing", "done"]),
          files_total: z.number().int().nonnegative().optional(),
          files_indexed: z.number().int().nonnegative().optional(),
          files_failed: z.number().int().nonnegative().optional(),
          detail: z.string().optional(),
        })
        .optional(),
      completion: z
        .object({
          completed: z.number().int().nonnegative(),
          total: z.number().int().nonnegative(),
        })
        .optional(),
      error: jobErrorSchema.optional(),
    })
    .optional(),
});

export const zvecGrepServerStatusOutputSchema = z.object({
  version: z.string(),
  uptime_ms: z.number().nonnegative(),
  shutting_down: z.boolean(),
  active_runtimes: z.number().int().nonnegative(),
  queued_jobs: z.number().int().nonnegative(),
  running_jobs: z.number().int().nonnegative(),
  models: z.object({
    loaded: z.number().int().nonnegative(),
    active_leases: z.number().int().nonnegative(),
  }),
});

export const legacySearchInputSchema = z.object({
  query: z
    .string()
    .optional()
    .describe("Natural-language or exact search query."),
  queries: legacyStringListInputSchema.describe(
    "Multiple query groups. Use this for related concepts.",
  ),
  fts: legacyStringListInputSchema.describe(
    "Exact lexical anchors, such as symbols, flags, or error messages.",
  ),
  vector: legacyStringListInputSchema.describe(
    "Explicit semantic/vector-only queries.",
  ),
  root: z
    .string()
    .optional()
    .describe("Repository root. Defaults to the MCP server working directory."),
  collection: z
    .string()
    .optional()
    .describe(
      "Named collection to query instead of the anonymous repository index.",
    ),
  limit: z
    .number()
    .int()
    .positive()
    .max(50)
    .optional()
    .describe("Maximum returned items per query/group."),
  include: legacyStringListInputSchema.describe(
    "Glob filters for paths to include. Accepts an array or comma-separated string.",
  ),
  exclude: legacyStringListInputSchema.describe(
    "Glob filters for paths to exclude. Accepts an array or comma-separated string.",
  ),
  preferSymbol: z
    .boolean()
    .optional()
    .describe("Prefer exact indexed symbols when query names a symbol."),
  symbolTypes: z
    .array(codeSymbolTypeSchema)
    .default([])
    .describe("Restrict indexed results to symbol types."),
  modifiedAfter: legacyTimeInputSchema.describe(
    "Only query files modified after this time. Accepts epoch milliseconds or a parseable date string.",
  ),
  modifiedBefore: legacyTimeInputSchema.describe(
    "Only query files modified before this time. Accepts epoch milliseconds or a parseable date string.",
  ),
  autoUpdate: z
    .boolean()
    .optional()
    .describe(
      "Refresh an existing stale anonymous index before query. Defaults to true.",
    ),
  embeddingConcurrency: z
    .number()
    .int()
    .positive()
    .max(64)
    .optional()
    .describe("Embedding task concurrency for automatic index refresh."),
  trace: z
    .boolean()
    .optional()
    .describe("Include per-hit search trace in structured output."),
});

export const legacyRgInputSchema = z.object({
  pattern: z
    .string()
    .optional()
    .describe("Regex or literal pattern to search for."),
  patterns: legacyStringListInputSchema.describe(
    "Multiple regex or literal patterns to search for.",
  ),
  root: z
    .string()
    .optional()
    .describe("Repository root. Defaults to the MCP server working directory."),
  paths: legacyStringListInputSchema.describe(
    "Optional paths to search within the root.",
  ),
  fixedStrings: z
    .boolean()
    .optional()
    .describe("Treat pattern as a literal string."),
  ignoreCase: z.boolean().optional().describe("Search case-insensitively."),
  wordRegexp: z.boolean().optional().describe("Only match whole words."),
  context: z
    .number()
    .int()
    .nonnegative()
    .max(20)
    .optional()
    .describe("Context lines before and after each match."),
  beforeContext: z
    .number()
    .int()
    .nonnegative()
    .max(20)
    .optional()
    .describe("Context lines before each match."),
  afterContext: z
    .number()
    .int()
    .nonnegative()
    .max(20)
    .optional()
    .describe("Context lines after each match."),
  hidden: z
    .boolean()
    .optional()
    .describe("Search hidden files and directories."),
  glob: legacyStringListInputSchema.describe(
    "ripgrep glob filters, for example '*.ts' or '!dist/**'. Accepts an array or comma-separated string.",
  ),
  limit: z
    .number()
    .int()
    .positive()
    .max(200)
    .optional()
    .describe("Maximum returned matches."),
});

export type ZvecGrepIndexInput = z.infer<typeof zvecGrepIndexInputSchema>;
export type ZvecGrepIndexDropInput = z.infer<
  typeof zvecGrepIndexDropInputSchema
>;
export type ZvecGrepSearchInput = z.infer<typeof zvecGrepSearchInputSchema>;
export type ZvecGrepIndexStatusInput = z.infer<
  typeof zvecGrepIndexStatusInputSchema
>;
export type ZvecGrepRgInput = z.infer<typeof zvecGrepRgInputSchema>;
export type LegacySearchInput = z.infer<typeof legacySearchInputSchema>;
export type LegacyRgInput = z.infer<typeof legacyRgInputSchema>;
export type StringListInput = z.infer<typeof stringListInputSchema>;
export type TimeInput = z.infer<typeof timeInputSchema>;
