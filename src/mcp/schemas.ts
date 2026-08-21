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
    .optional()
    .describe(description);

const pathFilter = z.string().max(MCP_MAX_PATH_CHARS);

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
  .describe("Absolute workspace root visible to the daemon.");

const embeddingSearchRuntimeFields = {
  apiKey: z
    .string()
    .min(1)
    .max(8_192)
    .optional()
    .describe("One-request embedding provider API key override."),
  device: z
    .enum(["auto", "cpu", "metal", "vulkan", "cuda"])
    .optional()
    .describe("One-request local embedding device override."),
};

const embeddingIndexRuntimeFields = {
  ...embeddingSearchRuntimeFields,
  endpoint: z
    .string()
    .url()
    .max(2_048)
    .optional()
    .describe("Remote embedding endpoint override."),
};

const searchFields = {
  ...embeddingSearchRuntimeFields,
  query: boundedString(
    "One primary hybrid-search group using natural-language or exact terms.",
  ).optional(),
  queries: boundedStringList(
    "One or more primary hybrid-search groups. By default, each group is searched separately and retains group metadata.",
  ),
  fts: boundedStringList(
    "Supplemental lexical-route groups for exact anchors such as symbols, flags, or error messages; these are retrieval routes, not hard result constraints.",
  ),
  vector: boundedStringList(
    "Supplemental semantic/vector-route groups; these are retrieval routes, not hard result constraints.",
  ),
  limit: z
    .number()
    .int()
    .positive()
    .max(MCP_MAX_SEARCH_LIMIT)
    .optional()
    .describe(
      "Maximum returned items per query group, or for the single fused plan.",
    ),
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
  noIgnore: z
    .boolean()
    .optional()
    .describe("Do not respect ignore files such as .gitignore."),
  ignoreFiles: pathFilterInputSchema.describe(
    "Additional ignore files relative to the workspace root.",
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
      "Collapse all primary and supplemental groups into one ranked search plan; otherwise search groups separately and retain group metadata.",
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
  ...embeddingIndexRuntimeFields,
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
  noIgnore: z
    .boolean()
    .optional()
    .describe("Do not respect ignore files such as .gitignore."),
  ignoreFiles: pathFilterInputSchema.describe(
    "Additional ignore files relative to the workspace root.",
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
  debug: z
    .boolean()
    .optional()
    .describe("Return skipped-file diagnostics after a completed index job."),
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

const cliSearchRouteInputSchema = z.object({
  mode: z.enum(["fts", "vector"]),
  query: boundedString(
    "One ordered supplemental retrieval group supplied by the CLI.",
  ),
});

/** Internal daemon-admin schema used to preserve CLI route argument order. */
export const zvecGrepCliSearchInputSchema = zvecGrepSearchInputSchema.extend({
  routes: z
    .array(cliSearchRouteInputSchema)
    .max(MCP_MAX_QUERY_GROUPS * 2)
    .optional()
    .describe(
      "Ordered supplemental retrieval groups. Internal CLI transport only.",
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
  root: absoluteRootSchema.describe(
    "Absolute workspace root visible to the daemon. Keep this at the workspace root; scope the search with command paths or globs.",
  ),
  command: z
    .string()
    .trim()
    .min(1, "command is required.")
    .max(MCP_MAX_QUERY_CHARS)
    .describe(
      "The command MUST start with `rg`; it is parsed as arguments and never executed by a shell. Search is exhaustive by default; append `| head -N` only to request a bounded result set.",
    ),
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
  matchedBy: z.enum(["fts", "vector", "fts+vector", "graph", "lexical"]),
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
  queryGroups: z
    .array(
      z.object({
        id: z.string(),
        query: z.string(),
        role: z.enum(["primary", "supplemental"]),
        rank: z.number().int().positive(),
        matchedBy: z.enum(["fts", "vector", "fts+vector"]),
      }),
    )
    .optional(),
  selectionReason: z.enum(["coverage", "global_fill"]).optional(),
  coverageGroup: z.string().optional(),
});
const contextGroupResultSchema = z.object({
  id: z.string(),
  query: z.string(),
  role: z.enum(["primary", "supplemental"]),
  items: z.array(contextItemSchema),
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
  workspaceIndex: z
    .object({
      id: z.string(),
      name: z.string(),
      path: z.string(),
    })
    .optional(),
  relationships: z
    .array(
      z.object({
        srcId: z.string(),
        dstId: z.string(),
        srcLabel: z.string(),
        dstLabel: z.string(),
        kind: z.enum(["CALLS", "REFS", "INHERITS", "CONTAINS", "IMPORTS"]),
        scope: z.enum(["symbol", "file"]),
      }),
    )
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
  groupResults: z.array(contextGroupResultSchema).optional(),
});

export const zvecGrepIndexOutputSchema = z.object({
  root: z.string(),
  job_id: z.string(),
  state: jobStateSchema,
  reused: z.boolean(),
  action: z.enum(["index", "drop"]).optional(),
  dropped: z.boolean().optional(),
  error: jobErrorSchema.optional(),
  scan_diagnostics: z
    .object({
      skippedFiles: z.number().int().nonnegative(),
      skippedByReason: z.object({
        empty: z.number().int().nonnegative(),
        too_large: z.number().int().nonnegative(),
        unsupported: z.number().int().nonnegative(),
        binary: z.number().int().nonnegative(),
      }),
      skippedSamples: z.array(
        z.object({
          absolutePath: z.string(),
          relativePath: z.string(),
          reason: z.enum(["empty", "too_large", "unsupported", "binary"]),
          sizeBytes: z.number().int().nonnegative().optional(),
          limitBytes: z.number().int().positive().optional(),
        }),
      ),
    })
    .optional(),
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
    workspace_index: z
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

export const zvecGrepExploreInputSchema = {
  root: absoluteRootSchema,
  query: boundedString(
    "Symbol name or short query used to seed the code-graph explore pack.",
  ),
  seedId: z
    .string()
    .min(1)
    .max(256)
    .optional()
    .describe("Disambiguate when multiple symbols match the query."),
  limit: z
    .number()
    .int()
    .positive()
    .max(32)
    .optional()
    .describe("Max seed symbols (default 8)."),
  depth: z
    .number()
    .int()
    .positive()
    .max(8)
    .optional()
    .describe("Graph traversal depth (default 3)."),
  maxFiles: z
    .number()
    .int()
    .positive()
    .max(32)
    .optional()
    .describe("Max files in the assembled context pack (default 8)."),
};

export const zvecGrepGraphNeighborhoodInputSchema = {
  root: absoluteRootSchema,
  query: boundedString("Exact symbol name or entity id."),
  seedId: z
    .string()
    .min(1)
    .max(256)
    .optional()
    .describe("Disambiguate when multiple symbols match the query."),
  depth: z
    .number()
    .int()
    .positive()
    .max(10)
    .optional()
    .describe("Traversal depth (default 1)."),
  limit: z
    .number()
    .int()
    .positive()
    .max(200)
    .optional()
    .describe("Max neighbors (default 20)."),
};

export type ZvecGrepIndexInput = z.infer<typeof zvecGrepIndexInputSchema>;
export type ZvecGrepIndexRequest = ZvecGrepIndexInput & {
  embeddingEnvironment?: string;
};
export type ZvecGrepIndexDropInput = z.infer<
  typeof zvecGrepIndexDropInputSchema
>;
export type ZvecGrepSearchInput = z.infer<typeof zvecGrepSearchInputSchema>;
export type ZvecGrepCliSearchInput = z.infer<
  typeof zvecGrepCliSearchInputSchema
>;
export type ZvecGrepIndexStatusInput = z.infer<
  typeof zvecGrepIndexStatusInputSchema
>;
export type ZvecGrepRgInput = z.infer<typeof zvecGrepRgInputSchema>;
export type StringListInput = z.infer<typeof stringListInputSchema>;
export type TimeInput = z.infer<typeof timeInputSchema>;
