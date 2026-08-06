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
import type { IndexProgress, ZvecGrepContextResult } from "../index.js";
import { formatAgentContextResult } from "../cli/format/context.js";
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
  zvecGrepSearchInputSchema,
  zvecGrepServerStatusInputSchema,
  zvecGrepServerStatusOutputSchema,
  type ZvecGrepIndexInput,
  type ZvecGrepIndexDropInput,
  type ZvecGrepIndexStatusInput,
  type ZvecGrepRgInput,
  type ZvecGrepSearchIndexing,
} from "./schemas.js";
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
    input: ZvecGrepIndexInput,
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
  planIndexAuthorization?(
    input: ZvecGrepIndexInput,
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

const ZVEC_GREP_AGENT_SEARCH_MCP_INSTRUCTIONS = [
  "Choose the initial search tool by the scope of the requested answer, not merely by whether the question contains an identifier.",
  "Use native Grep or rg first when the answer can be obtained by locating a specific definition, reference, filename, configuration key, error message, literal, source fragment, or regex.",
  "Use zvec_grep_search first when the answer requires architecture, lifecycle, system design, conceptual discovery, data or control flow, comparison across implementations, design rationale, or performance analysis.",
  "When exact symbols are present but the answer spans multiple components, files, stages, or implementations, treat it as a mixed task: use zvec_grep_search with the concept and known symbols, then use native Grep or rg for focused follow-up.",
  "Before using broad file reads or a sub-agent for conceptual repository discovery, make at least one appropriate zvec_grep_search call.",
  "Do not launch a sub-agent solely to locate code. Stop discovery when the evidence covers all components requested by the question.",
];

const ZVEC_GREP_FULL_SEARCH_MCP_INSTRUCTIONS = [
  "Choose the initial search tool by the scope of the requested answer, not merely by whether the question contains an identifier.",
  "Use zvec_grep_rg first only when the answer can be obtained by locating a specific definition, reference, filename, configuration key, error message, literal, source fragment, or regex.",
  "Use zvec_grep_search first when the answer requires architecture, lifecycle, system design, conceptual discovery, data or control flow, comparison across implementations, design rationale, or performance analysis.",
  "When exact symbols are present but the answer spans multiple components, files, stages, or implementations, treat it as a mixed task: search using the concept and known symbols, then use zvec_grep_rg for focused follow-up.",
  "Before using native Grep, Glob, shell rg, broad file reads, or a sub-agent for repository discovery, make at least one appropriate zvec-grep call.",
  "Do not launch a sub-agent solely to locate code. Stop discovery when the evidence covers all components requested by the question.",
];

export const ZVEC_GREP_AGENT_MCP_INSTRUCTIONS = [
  ...ZVEC_GREP_AGENT_SEARCH_MCP_INSTRUCTIONS,
  "Every repository operation requires an absolute root path visible to the daemon.",
  "Read freshness and indexing directly from zvec_grep_search responses without a status preflight.",
  "Use possibly_stale search results immediately when they are sufficient; do not perform extra diagnostics merely because a background update is active.",
  "When an index is missing and literal or regex search can answer the task, use native Grep or rg.",
].join(" ");

export const ZVEC_GREP_FULL_MCP_INSTRUCTIONS = [
  ...ZVEC_GREP_FULL_SEARCH_MCP_INSTRUCTIONS,
  "Every repository operation requires an absolute root path visible to the daemon.",
  "Use the zvec_grep_* tools directly for repository search, status, indexing, deletion, and exhaustive lexical search.",
  "Use freshness and indexing from zvec_grep_search without a status preflight; call zvec_grep_index_status only for a missing index, failed or cancelled indexing, diagnostics, or explicit progress monitoring.",
  "Use possibly_stale search results immediately when they are sufficient; do not call status merely because a background update is active.",
  "Call zvec_grep_index only when persistent indexing or index deletion is explicitly requested. Never silently create, rebuild, or drop an index.",
  "For a new index, use a user-selected embedding or omit it only when a server default model is known; never guess a model.",
  "zvec_grep_index wait defaults to false; poll zvec_grep_index_status for background progress and set wait to true only when completion is required before continuing.",
  "Use zvec_grep_index with drop: true, or zvec_grep_index_drop, only when index deletion is explicitly requested.",
  "Call zvec_grep_server_status only for daemon diagnostics, not before ordinary searches.",
].join(" ");

export const ZVEC_GREP_MCP_INSTRUCTIONS = ZVEC_GREP_AGENT_MCP_INSTRUCTIONS;

export type ZvecGrepMcpServerOptions = {
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
          "Activate an absolute repository root to create, incrementally update, rebuild, or explicitly drop its index. Do not call this tool to create, rebuild, or drop an index unless the user requested persistent indexing or index deletion.",
        inputSchema: zvecGrepIndexInputSchema,
        outputSchema: zvecGrepIndexOutputSchema,
        annotations: {
          readOnlyHint: false,
          destructiveHint: true,
          idempotentHint: true,
          openWorldHint: true,
        },
      },
      async (input, ctx) =>
        await runWithTraceContext(
          traceContextFromMcpMeta(ctx.mcpReq._meta),
          async () => {
            const plan = await backend.planIndexAuthorization?.(input);
            const resolution = plan
              ? await resolveRemoteEmbeddingAuthorization(
                  backend,
                  plan,
                  undefined,
                  ctx,
                  "zvec_grep_index",
                  input,
                  options,
                )
              : { kind: "ready" as const };
            if (resolution.kind === "input_required") return resolution.result;
            const progress = createMcpIndexProgressReporter(ctx);
            let result: ZvecGrepIndexResult;
            try {
              result = await backend.index(input, {
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
        ? "Search an existing repository index when the answer requires architecture, lifecycle, system design, conceptual discovery, data or control flow, comparison across implementations, design rationale, or performance analysis. Use search for mixed tasks whose question names exact symbols but whose answer spans multiple components, files, stages, or implementations; use managed ripgrep for focused follow-up. Read freshness and indexing from the response; use zvec_grep_index_status only for missing indexes, failed or cancelled indexing, diagnostics, or explicit progress monitoring."
        : "Search an existing repository index when the answer requires architecture, lifecycle, system design, conceptual discovery, data or control flow, comparison across implementations, design rationale, or performance analysis. Use search for mixed tasks whose question names exact symbols but whose answer spans multiple components, files, stages, or implementations; use native Grep or rg for focused follow-up. Read freshness and indexing directly from the response without a status preflight. When an index is unavailable, use the returned diagnostics and native Grep or rg for exact fallback.",
      inputSchema: zvecGrepSearchInputSchema,
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: false,
        openWorldHint: false,
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
              ? [`indexing: ${formatSearchIndexing(response.indexing)}`]
              : []),
          ];
          // Mirror the compact rg output: return agent-formatted text only and drop
          // the verbose structuredContent (per-item outline + full source), which
          // otherwise dominates the agent's context. `short` keeps a bounded source
          // snippet per hit so relevance is judgeable without extra file reads.
          return textToolResult(
            `${statusLines.join("\n")}\n${formatAgentContextResult(
              response.result,
              {
                preview: "short",
              },
            )}`,
          );
        },
      ),
  );

  if (full) {
    server.registerTool(
      "zvec_grep_index_drop",
      {
        title: "Drop zvec-grep Workspace index",
        description:
          "Delete the persisted index for an absolute Workspace root and release its daemon runtime.",
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
            ? `Dropped Workspace index for ${result.root}`
            : `No Workspace index found for ${result.root}`,
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
          "Run exhaustive, AST-enriched managed ripgrep locally without requiring an index. Use it first only when the answer can be obtained by locating a specific definition, reference, filename, configuration key, error message, literal, source fragment, or regex. Pass the rg command you would otherwise run. Results are exhaustive by default; append `| head -N` only when you intentionally want a bounded result set. Scope broad matches with command paths, `-g`/`--glob`, or `-t`/`--type` filters.",
        inputSchema: zvecGrepRgInputSchema,
        annotations: {
          readOnlyHint: true,
          destructiveHint: false,
          idempotentHint: true,
          openWorldHint: true,
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
  return {
    report(progress) {
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
