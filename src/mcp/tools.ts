import { randomBytes } from "node:crypto";
import {
  acceptedContent,
  inputRequired,
  inputResponse,
  McpServer,
  ProtocolError,
  type InputRequiredResult,
  type RequestStateCodec,
  type ServerContext,
} from "@modelcontextprotocol/server";
import { z } from "zod";
import type {
  FileScanDiagnostics,
  IndexProgress,
  ZvecGrepContextResult,
} from "../index.js";
import type {
  ZvecGrepExploreOptions,
  ZvecGrepExploreResult,
  ZvecGrepGraphNeighborhoodOptions,
  ZvecGrepGraphNeighborhoodResult,
} from "../engine/service/types.js";
import { formatAgentContextResult } from "../cli/format/context.js";
import {
  formatExploreResult,
  formatNeighborhoodResult,
} from "../presentation/graph.js";
import {
  formatRemoteEmbeddingAuthorizationPrompt,
  remoteEmbeddingDisclosureData,
} from "../authorization/prompt.js";
import {
  normalizeSearchInput,
  type NormalizedSearchInput,
} from "./input-normalization.js";
import {
  zvecGrepIndexInputSchema,
  zvecGrepIndexOutputSchema,
  zvecGrepIndexDropInputSchema,
  zvecGrepIndexDropOutputSchema,
  zvecGrepIndexStatusInputSchema,
  zvecGrepIndexStatusOutputSchema,
  zvecGrepRgInputSchema,
  zvecGrepCliSearchInputSchema,
  zvecGrepSearchInputSchema,
  zvecGrepSearchOutputSchema,
  zvecGrepServerStatusInputSchema,
  zvecGrepServerStatusOutputSchema,
  zvecGrepExploreInputSchema,
  zvecGrepGraphNeighborhoodInputSchema,
  type ZvecGrepIndexRequest,
  type ZvecGrepIndexDropInput,
  type ZvecGrepIndexStatusInput,
  type ZvecGrepRgInput,
  type ZvecGrepSearchIndexing,
} from "./schemas.js";
import { embeddingEnvironmentFromRequestMeta } from "./request-metadata.js";
import { textToolResult, toolResult } from "./result-format.js";
import type {
  RemoteEmbeddingAuthorizationPlan,
  RemoteEmbeddingAuthorizationScope,
  RemoteEmbeddingOperationPermit,
} from "../authorization/types.js";
import { indexProgressMessage } from "../index-progress.js";
import { DEFAULT_MCP_TOOLSET, type McpToolset } from "./toolset.js";
import {
  createRemoteEmbeddingRequestStateCodec,
  InMemoryRemoteEmbeddingRequestStateReplayGuard,
  matchesRemoteEmbeddingRequestState,
  remoteEmbeddingRequestState,
  type RemoteEmbeddingRequestStateReplayGuard,
  type RemoteEmbeddingRequestState,
} from "./request-state.js";
import {
  runWithTraceContext,
  traceContextFromMcpMeta,
} from "../observability/trace-context.js";
import {
  formatPromptRules,
  ZVEC_GREP_WORKSPACE_EVIDENCE_RULES,
} from "../prompts/zvec-grep-guidance.js";

export type IndexJobState =
  "queued" | "running" | "succeeded" | "failed" | "cancelled";

export type ZvecGrepIndexResult = {
  root: string;
  jobId: string;
  state: IndexJobState;
  reused: boolean;
  action?: "index" | "drop";
  dropped?: boolean;
  error?: { code: string; message: string };
  scanDiagnostics?: FileScanDiagnostics;
};

export type ZvecGrepIndexDropResult = {
  root: string;
  removed: boolean;
};

export type ZvecGrepSearchResult = {
  root: string;
  freshness: "fresh" | "possibly_stale";
  indexing?: ZvecGrepSearchIndexing;
  result: ZvecGrepContextResult;
};

export type ZvecGrepIndexStatusResult = {
  root: string;
  indexed: boolean;
  indexPolicy: "enabled" | "disabled" | "undecided";
  source: "index" | "unindexed";
  persistent: {
    home: string;
    index_path: string;
    workspace_index?: {
      id: string;
      name: string;
      path: string;
      root_paths: Array<{
        absolute_path: string;
        recursive: boolean;
        include?: string[];
        exclude?: string[];
        globs?: string[];
        insensitive_globs?: string[];
        file_types?: string[];
        excluded_file_types?: string[];
        hidden?: boolean;
        no_ignore?: boolean;
        ignore_files?: string[];
        max_depth?: number;
        max_file_size_bytes?: number;
        follow?: boolean;
      }>;
      embedding?: {
        provider: string;
        model: string;
        dimension: number;
        metric: string;
      } | null;
      index_version?: number | null;
      created_time: number;
      updated_time: number;
    };
    files?: {
      stored: number;
      scanned?: number;
      indexed: number;
      pending: number;
      failed: number;
      added: number;
      modified: number;
      deleted: number;
      unchanged: number;
      entities: number;
      truncated_fragments: number;
    };
    suggestion?: string;
  };
  runtime?: {
    watcherActive: boolean;
    dirtyRevision: number;
    indexedRevision: number;
    activeJobId?: string;
    jobState?: IndexJobState;
    progress?: {
      phase: "scanning" | "indexing" | "done";
      files_total?: number;
      files_indexed?: number;
      files_failed?: number;
      detail?: string;
    };
    completion?: {
      completed: number;
      total: number;
    };
    error?: { code: string; message: string };
  };
};

export type ZvecGrepServerStatusResult = {
  version: string;
  uptimeMs: number;
  shuttingDown: boolean;
  activeRuntimes: number;
  queuedJobs: number;
  runningJobs: number;
  models: {
    loaded: number;
    activeLeases: number;
  };
};

export type ZvecGrepRgResult = {
  root: string;
  result: ZvecGrepContextResult;
};

export interface ZvecGrepDaemonBackend {
  index(
    input: ZvecGrepIndexRequest,
    options?: {
      authorization?: RemoteEmbeddingOperationPermit;
      onProgress?: (progress: IndexProgress) => void;
    },
  ): Promise<ZvecGrepIndexResult>;
  dropIndex(input: ZvecGrepIndexDropInput): Promise<ZvecGrepIndexDropResult>;
  search(
    input: NormalizedSearchInput,
    options?: { authorization?: RemoteEmbeddingOperationPermit },
  ): Promise<ZvecGrepSearchResult>;
  explore(
    input: ZvecGrepExploreOptions & { root: string },
  ): Promise<ZvecGrepExploreResult>;
  graphNeighborhood(
    input: ZvecGrepGraphNeighborhoodOptions & { root: string },
  ): Promise<ZvecGrepGraphNeighborhoodResult>;
  planIndexAuthorization?(
    input: ZvecGrepIndexRequest,
  ): Promise<RemoteEmbeddingAuthorizationPlan | undefined>;
  planSearchAuthorization?(
    input: NormalizedSearchInput,
  ): Promise<RemoteEmbeddingAuthorizationPlan | undefined>;
  existingRemoteEmbeddingPermit?(
    plan: RemoteEmbeddingAuthorizationPlan,
  ): Promise<RemoteEmbeddingOperationPermit | undefined>;
  grantRemoteEmbedding?(
    plan: RemoteEmbeddingAuthorizationPlan,
    scope: RemoteEmbeddingAuthorizationScope,
  ): Promise<RemoteEmbeddingOperationPermit>;
  indexStatus(
    input: ZvecGrepIndexStatusInput,
  ): Promise<ZvecGrepIndexStatusResult>;
  rg(input: ZvecGrepRgInput): Promise<ZvecGrepRgResult>;
  serverStatus(): Promise<ZvecGrepServerStatusResult>;
}

function searchRoutingRules(exactTool: string, focusedTools: string): string[] {
  return [
    `Use ${exactTool} first only when exact lookup alone is sufficient, such as locating one definition, literal, filename, configuration key, error message, regex match, or exhaustive occurrence list.`,
    "Use zvec_grep_search first when wording or location is unknown, or when the answer requires architecture, lifecycle, call relationships, dependencies, data or control flow, design rationale, comparison, or synthesis across files or components.",
    "Use zvec_grep_explore when you already have a symbol/name and need a multi-file call/type-neighborhood context pack assembled from the graph.",
    "Use zvec_grep_callers, zvec_grep_callees, or zvec_grep_impact for focused graph neighborhood questions about one symbol.",
    `When user-provided or verified exact symbols are present but the answer spans multiple files, components, stages, implementations, or relationships, treat the task as mixed: call zvec_grep_search with the semantic intent and those anchors, then use ${focusedTools} for focused verification.`,
    "For a semantic or mixed workspace task, start discovery with focused zvec_grep_search before broad file discovery.",
    "Preserve the question's concepts, relationships, and constraints from the user request and established context in semantic queries. Treat inferred names as supplemental hypotheses, not replacements for or constraints on the stated intent.",
    "`query` creates one primary hybrid FTS-plus-vector group; `queries` creates one or more primary hybrid groups; `fts` and `vector` add supplemental lexical-only or semantic-only route groups. These are retrieval routes, not hard constraints. Without `fuse`, the response is one deduplicated and reranked list with query-group metadata; set `fuse: true` to collapse every group into one ranked search plan.",
    'For a fused mixed search, use arguments such as {"root":"/absolute/workspace","query":"how are results ranked and fused","fts":["RRF","score"],"fuse":true}.',
    "Search results include bounded source snippets. Treat a sufficient snippet as already-read evidence, and open only the cited file or range when a required detail falls outside it.",
    "Evidence is sufficient when it matches the requested answer shape and resolves material ambiguity. A chain, flow, or lifecycle requires the relevant path and material transitions, not an isolated match. When a task requires one target and several candidates plausibly fit, compare them; if they remain indistinguishable, state the ambiguity.",
    `Use the smallest focused follow-up needed to fill a material coverage gap or distinguish plausible candidates: refine zvec_grep_search, or verify with ${focusedTools}. If semantic retrieval remains irrelevant, fall back to ${exactTool}. Stop once the requested scope is supported; do not continue merely to reconfirm or broaden it.`,
    "Do not launch a sub-agent solely to locate workspace material.",
  ];
}

const ZVEC_GREP_AGENT_SEARCH_MCP_INSTRUCTIONS = searchRoutingRules(
  "native Grep or rg",
  "Read, Grep, or rg",
);

const ZVEC_GREP_FULL_SEARCH_MCP_INSTRUCTIONS = searchRoutingRules(
  "zvec_grep_rg",
  "Read or zvec_grep_rg",
);

const ZVEC_GREP_SEARCH_TOOL_DESCRIPTION =
  "Search an existing workspace index for semantic, relational, cross-file, or multi-hop evidence such as architecture, call chains, dependencies, lifecycle, data or control flow, design rationale, and comparisons. Use it when exact lookup alone cannot answer a workspace-grounded question. Results include bounded source snippets and query-group metadata; treat sufficient snippets as already-read evidence.";

export const ZVEC_GREP_AGENT_MCP_INSTRUCTIONS = formatPromptRules(
  "Use zvec-grep with these workspace retrieval rules:",
  [
    ...ZVEC_GREP_WORKSPACE_EVIDENCE_RULES,
    ...ZVEC_GREP_AGENT_SEARCH_MCP_INSTRUCTIONS,
    "Every workspace operation requires an absolute root path visible to the daemon.",
    "Read freshness and background_refresh directly from zvec_grep_search responses without a status preflight.",
    "When results are served_from_current_index, use them immediately when they are sufficient; do not perform extra diagnostics merely because a background refresh is active.",
    "When an index is missing and literal or regex search can answer the task, use native Grep or rg. Creating or rebuilding a persistent index requires explicit user authorization.",
  ],
);

export const ZVEC_GREP_FULL_MCP_INSTRUCTIONS = formatPromptRules(
  "Use zvec-grep with these workspace retrieval and lifecycle rules:",
  [
    ...ZVEC_GREP_WORKSPACE_EVIDENCE_RULES,
    ...ZVEC_GREP_FULL_SEARCH_MCP_INSTRUCTIONS,
    "Every workspace operation requires an absolute root path visible to the daemon.",
    "Use the zvec_grep_* tools directly for workspace search, status, indexing, deletion, and exhaustive lexical search.",
    "Use freshness and background_refresh from zvec_grep_search without a status preflight; call zvec_grep_index_status only for a missing index, failed or cancelled indexing, diagnostics, or explicit progress monitoring.",
    "When results are served_from_current_index, use them immediately when they are sufficient; do not call status merely because a background refresh is active.",
    "Call zvec_grep_index only when persistent indexing or index deletion is explicitly requested. Never silently create, rebuild, or drop an index.",
    "For a new index, use a user-selected embedding or omit it only when a server default model is known; never guess a model.",
    "zvec_grep_index wait defaults to false; poll zvec_grep_index_status for background progress and set wait to true only when completion is required before continuing.",
    "Use zvec_grep_index with drop: true, or zvec_grep_index_drop, only when index deletion is explicitly requested.",
    "Call zvec_grep_server_status only for daemon diagnostics, not before ordinary searches.",
  ],
);

export const ZVEC_GREP_MCP_INSTRUCTIONS = ZVEC_GREP_AGENT_MCP_INSTRUCTIONS;

export type ZvecGrepMcpServerOptions = {
  acceptEmbeddingEnvironmentMeta?: boolean;
  /** Internal CLI transport only; public MCP search remains compact text. */
  includeSearchStructuredContent?: boolean;
  requestStateCodec?: RequestStateCodec<RemoteEmbeddingRequestState>;
  requestStateReplayGuard?: RemoteEmbeddingRequestStateReplayGuard;
  toolset?: McpToolset;
};

export function createZvecGrepMcpServer(
  backend: ZvecGrepDaemonBackend,
  version: string,
  options: ZvecGrepMcpServerOptions = {},
): McpServer {
  const toolset = options.toolset ?? DEFAULT_MCP_TOOLSET;
  const requestStateCodec =
    options.requestStateCodec ??
    createRemoteEmbeddingRequestStateCodec(randomBytes(32));
  const requestStateReplayGuard =
    options.requestStateReplayGuard ??
    new InMemoryRemoteEmbeddingRequestStateReplayGuard();
  const server = new McpServer(
    { name: "zvec-grep", version },
    {
      instructions:
        toolset === "full"
          ? ZVEC_GREP_FULL_MCP_INSTRUCTIONS
          : ZVEC_GREP_AGENT_MCP_INSTRUCTIONS,
      cacheHints: {
        "server/discover": { ttlMs: 60 * 60 * 1_000, cacheScope: "private" },
        "tools/list": { ttlMs: 60 * 60 * 1_000, cacheScope: "private" },
      },
      requestState: { verify: requestStateCodec.verify },
    },
  );
  registerZvecGrepTools(server, backend, {
    ...options,
    requestStateCodec,
    requestStateReplayGuard,
    toolset,
  });
  return server;
}

export function registerZvecGrepTools(
  server: McpServer,
  backend: ZvecGrepDaemonBackend,
  options: ZvecGrepMcpServerOptions = {},
): void {
  const toolset = options.toolset ?? DEFAULT_MCP_TOOLSET;
  const full = toolset === "full";

  if (full) {
    server.registerTool(
      "zvec_grep_index",
      {
        title: "Ensure or drop zvec-grep index",
        description:
          "Activate an absolute workspace root to create, incrementally update, rebuild, or explicitly drop its index. Do not call this tool to create, rebuild, or drop an index unless the user requested persistent indexing or index deletion.",
        inputSchema: zvecGrepIndexInputSchema,
        outputSchema: zvecGrepIndexOutputSchema,
        annotations: {
          readOnlyHint: false,
          destructiveHint: true,
          idempotentHint: false,
          openWorldHint: true,
        },
      },
      async (input, ctx) =>
        await runWithTraceContext(
          traceContextFromMcpMeta(ctx.mcpReq._meta),
          async () => {
            const request: ZvecGrepIndexRequest = {
              ...input,
              embeddingEnvironment: options.acceptEmbeddingEnvironmentMeta
                ? embeddingEnvironmentFromRequestMeta(ctx.mcpReq._meta)
                : undefined,
            };
            const plan = await backend.planIndexAuthorization?.(request);
            const resolution = plan
              ? await resolveRemoteEmbeddingAuthorization(
                  backend,
                  plan,
                  undefined,
                  ctx,
                  "zvec_grep_index",
                  request,
                  options,
                )
              : { kind: "ready" as const };
            if (resolution.kind === "input_required") return resolution.result;
            const progress = createMcpIndexProgressReporter(ctx);
            let result: ZvecGrepIndexResult;
            try {
              result = await backend.index(request, {
                authorization: resolution.authorization,
                onProgress: progress.report,
              });
            } finally {
              await progress.flush();
            }
            const structuredContent = {
              root: result.root,
              job_id: result.jobId,
              state: result.state,
              reused: result.reused,
              action: result.action,
              dropped: result.dropped,
              error: result.error,
              scan_diagnostics: result.scanDiagnostics,
            };
            return toolResult(
              [
                `root: ${result.root}`,
                `job_id: ${result.jobId}`,
                `state: ${result.state}`,
                `reused: ${result.reused}`,
                ...(result.action ? [`action: ${result.action}`] : []),
                ...(result.dropped !== undefined
                  ? [`dropped: ${result.dropped}`]
                  : []),
                ...(result.error
                  ? [
                      `error_code: ${result.error.code}`,
                      `error_message: ${result.error.message}`,
                    ]
                  : []),
                ...(result.scanDiagnostics
                  ? [`skipped_files: ${result.scanDiagnostics.skippedFiles}`]
                  : []),
              ].join("\n"),
              structuredContent,
            );
          },
        ),
    );
  }

  server.registerTool(
    "zvec_grep_search",
    {
      title: "Search with zvec-grep",
      description: full
        ? `${ZVEC_GREP_SEARCH_TOOL_DESCRIPTION} Use zvec_grep_rg instead when exact lookup alone is sufficient. Read freshness and background_refresh from the response; when results are served_from_current_index, use them if sufficient.`
        : `${ZVEC_GREP_SEARCH_TOOL_DESCRIPTION} Use native Grep or rg instead when exact lookup alone is sufficient. Read freshness and background_refresh from the response without a status preflight; when results are served_from_current_index, use them if sufficient.`,
      inputSchema: options.includeSearchStructuredContent
        ? zvecGrepCliSearchInputSchema
        : zvecGrepSearchInputSchema,
      ...(options.includeSearchStructuredContent
        ? { outputSchema: zvecGrepSearchOutputSchema }
        : {}),
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: false,
        openWorldHint: true,
      },
    },
    async (input, ctx) =>
      await runWithTraceContext(
        traceContextFromMcpMeta(ctx.mcpReq._meta),
        async () => {
          const normalized = normalizeSearchInput(input);
          const plan = await backend.planSearchAuthorization?.(normalized);
          const resolution = plan
            ? await resolveRemoteEmbeddingAuthorization(
                backend,
                plan,
                "local_search",
                ctx,
                "zvec_grep_search",
                normalized,
                options,
              )
            : { kind: "ready" as const };
          if (resolution.kind === "input_required") return resolution.result;
          const effectiveSearch =
            resolution.alternative === "local_search"
              ? ftsFallbackSearch(normalized)
              : normalized;
          const response = await backend.search(effectiveSearch, {
            authorization: resolution.authorization,
          });
          const statusLines = [
            `freshness: ${response.freshness}`,
            ...(response.indexing
              ? [
                  "results: served_from_current_index",
                  `background_refresh: ${formatSearchIndexing(response.indexing)}`,
                ]
              : []),
          ];
          // Mirror the compact rg output: return agent-formatted text only and drop
          // the verbose structuredContent (per-item outline + full source), which
          // otherwise dominates the agent's context. `short` keeps a bounded source
          // snippet per hit so relevance is judgeable without extra file reads.
          const text = `${statusLines.join("\n")}\n${formatAgentContextResult(
            response.result,
            {
              preview: "short",
            },
          )}`;
          return options.includeSearchStructuredContent
            ? toolResult(text, {
                root: response.root,
                freshness: response.freshness,
                indexing: response.indexing,
                // The CLI renders per-group recall and does not consume the
                // cross-group list. Avoid serializing every full item twice.
                result: {
                  ...response.result,
                  items: [],
                  groupResults: response.result.groupResults?.map((group) => ({
                    ...group,
                    items: group.items.map(
                      ({
                        queryGroups: _queryGroups,
                        selectionReason: _selectionReason,
                        coverageGroup: _coverageGroup,
                        ...item
                      }) => item,
                    ),
                  })),
                },
              })
            : textToolResult(text);
        },
      ),
  );

  server.registerTool(
    "zvec_grep_explore",
    {
      title: "Explore code-graph context",
      description:
        "Build a multi-file context pack from the workspace code graph for a symbol or short query: hierarchy + deep neighborhood + ranked file assembly of indexed entity source.",
      inputSchema: zvecGrepExploreInputSchema,
      annotations: {
        readOnlyHint: true,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: false,
      },
    },
    async (input) => {
      const result = await backend.explore({
        query: input.query,
        seedId: input.seedId,
        searchLimit: input.limit,
        traversalDepth: input.depth,
        maxFiles: input.maxFiles,
        root: input.root,
      });
      return textToolResult(formatExploreResult(result));
    },
  );

  for (const direction of ["callers", "callees", "impact"] as const) {
    server.registerTool(
      `zvec_grep_${direction}`,
      {
        title: `Code-graph ${direction}`,
        description: `List ${direction} of a symbol from the workspace code graph.`,
        inputSchema: zvecGrepGraphNeighborhoodInputSchema,
        annotations: {
          readOnlyHint: true,
          destructiveHint: false,
          idempotentHint: true,
          openWorldHint: false,
        },
      },
      async (input) => {
        const result = await backend.graphNeighborhood({
          direction,
          query: input.query,
          seedId: input.seedId,
          depth: input.depth,
          limit: input.limit,
          root: input.root,
        });
        return textToolResult(formatNeighborhoodResult(result));
      },
    );
  }

  if (full) {
    server.registerTool(
      "zvec_grep_index_drop",
      {
        title: "Drop zvec-grep workspace index",
        description:
          "Delete the persisted index for an absolute workspace root and release its daemon runtime.",
        inputSchema: zvecGrepIndexDropInputSchema,
        outputSchema: zvecGrepIndexDropOutputSchema,
        annotations: {
          readOnlyHint: false,
          destructiveHint: true,
          idempotentHint: true,
          openWorldHint: false,
        },
      },
      async (input) => {
        const result = await backend.dropIndex(input);
        const structuredContent = {
          root: result.root,
          removed: result.removed,
        };
        return toolResult(
          result.removed
            ? `Dropped workspace index for ${result.root}`
            : `No workspace index found for ${result.root}`,
          structuredContent,
        );
      },
    );
  }

  if (full) {
    server.registerTool(
      "zvec_grep_rg",
      {
        title: "Search with managed ripgrep",
        description:
          "Run exhaustive, AST-enriched ripgrep across code or non-code workspace material without an index. Use it when a known word, symbol, filename, source fragment, or regex can answer the workspace-grounded question. Pass a command starting with `rg`; results are exhaustive unless a trailing `| head -N` explicitly bounds them.",
        inputSchema: zvecGrepRgInputSchema,
        annotations: {
          readOnlyHint: true,
          destructiveHint: false,
          idempotentHint: true,
          openWorldHint: false,
        },
      },
      async (input) => {
        const response = await backend.rg(input);
        return textToolResult(
          `${formatAgentContextResult(response.result, {})}${rgCoverageHint(
            response.result,
          )}`,
        );
      },
    );
  }

  if (full) {
    server.registerTool(
      "zvec_grep_index_status",
      {
        title: "Inspect zvec-grep index status",
        description:
          "Read persisted index status and, when active, daemon runtime and job status for an absolute root. Use only after a missing-index response, indexing failure or cancellation, explicit progress monitoring, or daemon diagnostics.",
        inputSchema: zvecGrepIndexStatusInputSchema,
        outputSchema: zvecGrepIndexStatusOutputSchema,
        annotations: {
          readOnlyHint: true,
          destructiveHint: false,
          idempotentHint: true,
          openWorldHint: false,
        },
      },
      async (input) => {
        const result = await backend.indexStatus(input);
        const structuredContent = formatIndexStatus(result);
        return toolResult(
          JSON.stringify(structuredContent, null, 2),
          structuredContent,
        );
      },
    );

    server.registerTool(
      "zvec_grep_server_status",
      {
        title: "Inspect zvec-grep server status",
        description:
          "Read daemon version, queue, runtime and model-pool summary without exposing repository paths.",
        inputSchema: zvecGrepServerStatusInputSchema,
        outputSchema: zvecGrepServerStatusOutputSchema,
        annotations: {
          readOnlyHint: true,
          destructiveHint: false,
          idempotentHint: true,
          openWorldHint: false,
        },
      },
      async () => {
        const result = await backend.serverStatus();
        const structuredContent = {
          version: result.version,
          uptime_ms: result.uptimeMs,
          shutting_down: result.shuttingDown,
          active_runtimes: result.activeRuntimes,
          queued_jobs: result.queuedJobs,
          running_jobs: result.runningJobs,
          models: {
            loaded: result.models.loaded,
            active_leases: result.models.activeLeases,
          },
        };
        return toolResult(
          JSON.stringify(structuredContent, null, 2),
          structuredContent,
        );
      },
    );
  }
}

function rgCoverageHint(result: ZvecGrepContextResult): string {
  return result.coverage === "rg_truncated"
    ? "\n\nMore matches were omitted by the explicit output bound. Remove or increase the trailing `head` bound to see them."
    : "";
}

function createMcpIndexProgressReporter(extra: ServerContext): {
  report: (progress: IndexProgress) => void;
  flush: () => Promise<void>;
} {
  const progressToken = extra.mcpReq._meta?.progressToken;
  let progressValue = Date.now();
  let pending = Promise.resolve();
  let modelPreparationActive = false;
  return {
    report(progress) {
      const modelStage = progress.embedding?.stage;
      if (modelStage === "preparing" || modelStage === "downloading") {
        modelPreparationActive = true;
      } else if (modelStage === "ready" || progress.phase === "done") {
        modelPreparationActive = false;
      } else if (
        modelPreparationActive &&
        modelStage === undefined &&
        progress.phase === "indexing"
      ) {
        return;
      }
      const message = indexProgressMessage(progress);
      if (
        progressToken === undefined ||
        !message ||
        extra.mcpReq.signal.aborted
      ) {
        return;
      }
      progressValue = Math.max(progressValue + 1, Date.now());
      pending = pending
        .then(() =>
          extra.mcpReq.notify({
            method: "notifications/progress",
            params: {
              progressToken,
              progress: progressValue,
              message,
            },
          }),
        )
        .catch(() => undefined);
    },
    async flush() {
      await pending;
    },
  };
}

const REMOTE_EMBEDDING_SCOPE_DESCRIPTION =
  "Allow this operation once or remember permission for this workspace.";

const remoteEmbeddingDecisionSchema = z.object({
  decision: z.enum([
    "allow_once",
    "allow_workspace",
    "use_local_search",
    "cancel",
  ]),
});

type RemoteEmbeddingAuthorizationResolution =
  | {
      kind: "ready";
      authorization?: RemoteEmbeddingOperationPermit;
      alternative?: "local_search";
    }
  | { kind: "input_required"; result: InputRequiredResult };

async function resolveRemoteEmbeddingAuthorization(
  backend: ZvecGrepDaemonBackend,
  plan: RemoteEmbeddingAuthorizationPlan,
  alternative: "local_search" | undefined,
  ctx: ServerContext,
  tool: string,
  args: unknown,
  options: ZvecGrepMcpServerOptions = {},
): Promise<RemoteEmbeddingAuthorizationResolution> {
  const existing = await backend.existingRemoteEmbeddingPermit?.(plan);
  if (existing) return { kind: "ready", authorization: existing };
  if (!backend.grantRemoteEmbedding) {
    throw new Error("Remote Embedding authorization is required.");
  }

  const expectedState = remoteEmbeddingRequestState(tool, args, plan);
  const echoedState = ctx.mcpReq.requestState<RemoteEmbeddingRequestState>();
  if (echoedState) {
    if (!matchesRemoteEmbeddingRequestState(echoedState, expectedState)) {
      throw new ProtocolError(-32602, "Invalid or expired requestState");
    }
    const response = inputResponse(
      ctx.mcpReq.inputResponses,
      "remote_embedding_authorization",
    );
    const content = acceptedContent(
      ctx.mcpReq.inputResponses,
      "remote_embedding_authorization",
      remoteEmbeddingDecisionSchema,
    );
    if (
      response.kind !== "elicit" ||
      response.action !== "accept" ||
      !content
    ) {
      throw new Error(
        "Remote Embedding authorization was declined. No remote data was sent.",
      );
    }
    if (!(await options.requestStateReplayGuard?.consume(echoedState))) {
      throw new ProtocolError(-32602, "Invalid or expired requestState");
    }
    if (content.decision === "use_local_search" && alternative) {
      return { kind: "ready", alternative: "local_search" };
    }
    if (
      content.decision !== "allow_once" &&
      content.decision !== "allow_workspace"
    ) {
      throw new Error(
        "Remote Embedding authorization was declined. No remote data was sent.",
      );
    }
    const scope: RemoteEmbeddingAuthorizationScope =
      content.decision === "allow_workspace" ? "workspace" : "once";
    return {
      kind: "ready",
      authorization: await backend.grantRemoteEmbedding(plan, scope),
    };
  }

  const requestStateCodec = options.requestStateCodec;
  if (!requestStateCodec) {
    throw new Error("MCP request-state signing is not configured.");
  }
  return {
    kind: "input_required",
    result: inputRequired({
      requestState: await requestStateCodec.mint(
        options.requestStateReplayGuard!.issue(expectedState),
        ctx,
      ),
      inputRequests: {
        remote_embedding_authorization: inputRequired.elicit({
          mode: "form",
          message: formatRemoteEmbeddingAuthorizationPrompt({
            workspaceRoots: plan.target.workspaceRoots,
            provider: plan.target.provider,
            model: plan.target.model,
            endpoint: plan.target.endpoint,
            data: remoteEmbeddingDisclosureData(plan.disclosure),
          }),
          requestedSchema: {
            type: "object",
            properties: {
              decision: {
                type: "string",
                title: "Remote Embedding permission",
                description: REMOTE_EMBEDDING_SCOPE_DESCRIPTION,
                oneOf: [
                  { const: "allow_once", title: "Allow once" },
                  {
                    const: "allow_workspace",
                    title: "Allow for this workspace",
                  },
                  ...(alternative === "local_search"
                    ? [
                        {
                          const: "use_local_search",
                          title: "Use FTS only",
                        },
                      ]
                    : []),
                  { const: "cancel", title: "Cancel" },
                ],
                default: "cancel",
              },
            },
            required: ["decision"],
          },
        }),
      },
    }),
  };
}

function ftsFallbackSearch(
  input: NormalizedSearchInput,
): NormalizedSearchInput {
  const queries = [
    ...(input.queries ?? []),
    ...input.routes.map((route) => route.query),
  ];
  return {
    ...input,
    queries: undefined,
    routes: queries.map((query) => ({ mode: "fts" as const, query })),
    autoUpdate: false,
    freshness: "eventual",
  };
}

function formatSearchIndexing(
  indexing: NonNullable<ZvecGrepSearchResult["indexing"]>,
): string {
  if (indexing.completed === undefined || indexing.total === undefined) {
    return indexing.state;
  }
  return `${indexing.state} (${indexing.completed}/${indexing.total})`;
}

function formatIndexStatus(
  result: ZvecGrepIndexStatusResult,
): Record<string, unknown> {
  return {
    root: result.root,
    indexed: result.indexed,
    index_policy: result.indexPolicy,
    source: result.source,
    persistent: result.persistent,
    runtime: result.runtime
      ? {
          watcher_active: result.runtime.watcherActive,
          dirty_revision: result.runtime.dirtyRevision,
          indexed_revision: result.runtime.indexedRevision,
          active_job_id: result.runtime.activeJobId,
          job_state: result.runtime.jobState,
          progress: result.runtime.progress,
          completion: result.runtime.completion,
          error: result.runtime.error,
        }
      : undefined,
  };
}
