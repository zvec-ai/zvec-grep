import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { RequestHandlerExtra } from "@modelcontextprotocol/sdk/shared/protocol.js";
import type {
  ServerNotification,
  ServerRequest,
} from "@modelcontextprotocol/sdk/types.js";
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
import {
  LONG_RUNNING_MCP_TIMEOUT_MS,
  REMOTE_AUTHORIZATION_HEARTBEAT_MS,
  withProgressHeartbeat,
} from "./progress-heartbeat.js";
import { indexProgressMessage } from "../index-progress.js";
import { DEFAULT_MCP_TOOLSET, type McpToolset } from "./toolset.js";

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
    collection?: {
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

export const ZVEC_GREP_MCP_INSTRUCTIONS = [
  "Use zvec-grep for local workspace search instead of raw grep, rg, or equivalent local text-search tools.",
  "zvec-grep is a more capable superset replacement for rg.",
  "When the exact keyword, text, or symbol is unknown, start with zvec_grep_search to quickly identify relevant concepts, files, and locations.",
  "When the exact keyword, text, or symbol is known, use zvec_grep_rg.",
  "Scope searches with paths or globs when you already know likely locations or file types, and refine broad or noisy searches by narrowing the query and search scope.",
  "Trust zvec-grep results; if results are too broad, sparse, or low-quality, refine the query, scope, or search options and try zvec-grep again instead of switching to another local text-search tool.",
].join(" ");

export const ZVEC_GREP_AGENT_MCP_INSTRUCTIONS = ZVEC_GREP_MCP_INSTRUCTIONS;

export const ZVEC_GREP_FULL_MCP_INSTRUCTIONS = ZVEC_GREP_MCP_INSTRUCTIONS;

export type ZvecGrepMcpServerOptions = {
  authorizationHeartbeatMs?: number;
  authorizationRequestTimeoutMs?: number;
  toolset?: McpToolset;
};

export function createZvecGrepMcpServer(
  backend: ZvecGrepDaemonBackend,
  version: string,
  options: ZvecGrepMcpServerOptions = {},
): McpServer {
  const toolset = options.toolset ?? DEFAULT_MCP_TOOLSET;
  const server = new McpServer(
    { name: "zvec-grep", version },
    {
      instructions:
        toolset === "full"
          ? ZVEC_GREP_FULL_MCP_INSTRUCTIONS
          : ZVEC_GREP_AGENT_MCP_INSTRUCTIONS,
    },
  );
  registerZvecGrepTools(server, backend, { ...options, toolset });
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
        inputSchema: zvecGrepIndexInputSchema.shape,
        outputSchema: zvecGrepIndexOutputSchema.shape,
        annotations: {
          readOnlyHint: false,
          destructiveHint: true,
          idempotentHint: true,
          openWorldHint: true,
        },
      },
      async (input, extra) => {
        const plan = await backend.planIndexAuthorization?.(input);
        const resolution = plan
          ? await resolveRemoteEmbeddingAuthorization(
              server,
              backend,
              plan,
              undefined,
              extra,
              options,
            )
          : {};
        const progress = createMcpIndexProgressReporter(extra);
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
    );
  }

  server.registerTool(
    "zvec_grep_search",
    {
      title: "Search with zvec-grep",
      description: full
        ? "Search an existing repository index only when the exact keyword, text, symbol, filename, or path is unknown and conceptual discovery is needed. A known class, function, or symbol name is an exact anchor even when its file or definition location is unknown; use the managed ripgrep tool instead. Read freshness and indexing from the response; use zvec_grep_index_status only for missing indexes, failed or cancelled indexing, diagnostics, or explicit progress monitoring."
        : "Search an existing repository index only when the exact keyword, text, symbol, filename, or path is unknown and conceptual discovery is needed. A known class, function, or symbol name is an exact anchor even when its file or definition location is unknown; use the managed ripgrep tool instead. Read freshness and indexing directly from the response without a status preflight. When an index is unavailable, use the returned diagnostics to decide whether managed ripgrep can answer the task.",
      inputSchema: zvecGrepSearchInputSchema.shape,
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: false,
        openWorldHint: false,
      },
    },
    async (input, extra) => {
      const normalized = normalizeSearchInput(input);
      const plan = await backend.planSearchAuthorization?.(normalized);
      const resolution = plan
        ? await resolveRemoteEmbeddingAuthorization(
            server,
            backend,
            plan,
            "local_search",
            extra,
            options,
          )
        : {};
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
  );

  if (full) {
    server.registerTool(
      "zvec_grep_index_drop",
      {
        title: "Drop zvec-grep Workspace index",
        description:
          "Delete the persisted index for an absolute Workspace root and release its daemon runtime.",
        inputSchema: zvecGrepIndexDropInputSchema.shape,
        outputSchema: zvecGrepIndexDropOutputSchema.shape,
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

  server.registerTool(
    "zvec_grep_rg",
    {
      title: "Search with managed ripgrep",
      description:
        "Run exhaustive managed ripgrep locally without requiring an index. Use it first when an exact keyword, text, symbol, filename, path, configuration key, error message, source fragment, literal, or regex anchor is known. A named class, function, or symbol remains an exact anchor even when its file or definition location is unknown. Scope broad matches with paths or globs.",
      inputSchema: zvecGrepRgInputSchema.shape,
      annotations: {
        readOnlyHint: true,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    async (input) => {
      const response = await backend.rg(input);
      return textToolResult(formatAgentContextResult(response.result, {}));
    },
  );

  if (full) {
    server.registerTool(
      "zvec_grep_index_status",
      {
        title: "Inspect zvec-grep index status",
        description:
          "Read persisted index status and, when active, daemon runtime and job status for an absolute root. Use only after a missing-index response, indexing failure or cancellation, explicit progress monitoring, or daemon diagnostics.",
        inputSchema: zvecGrepIndexStatusInputSchema.shape,
        outputSchema: zvecGrepIndexStatusOutputSchema.shape,
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
        inputSchema: zvecGrepServerStatusInputSchema.shape,
        outputSchema: zvecGrepServerStatusOutputSchema.shape,
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

function createMcpIndexProgressReporter(
  extra: RequestHandlerExtra<ServerRequest, ServerNotification>,
): {
  report: (progress: IndexProgress) => void;
  flush: () => Promise<void>;
} {
  const progressToken = extra._meta?.progressToken;
  let progressValue = Date.now();
  let pending = Promise.resolve();
  return {
    report(progress) {
      const message = indexProgressMessage(progress);
      if (progressToken === undefined || !message || extra.signal.aborted) {
        return;
      }
      progressValue = Math.max(progressValue + 1, Date.now());
      pending = pending
        .then(() =>
          extra.sendNotification({
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

async function resolveRemoteEmbeddingAuthorization(
  server: McpServer,
  backend: ZvecGrepDaemonBackend,
  plan: RemoteEmbeddingAuthorizationPlan,
  alternative: "local_search" | undefined,
  extra: RequestHandlerExtra<ServerRequest, ServerNotification>,
  options: ZvecGrepMcpServerOptions = {},
): Promise<{
  authorization?: RemoteEmbeddingOperationPermit;
  alternative?: "local_search";
}> {
  const existing = await backend.existingRemoteEmbeddingPermit?.(plan);
  if (existing) return { authorization: existing };
  if (!backend.grantRemoteEmbedding) {
    throw new Error("Remote Embedding authorization is required.");
  }

  const elicitation = await elicitRemoteEmbeddingAuthorization(
    server,
    extra,
    options,
    {
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
    },
  );
  const decision =
    elicitation.action === "accept" ? elicitation.content?.decision : undefined;
  if (decision === "use_local_search") {
    return { alternative: "local_search" };
  }
  if (decision !== "allow_once" && decision !== "allow_workspace") {
    throw new Error(
      "Remote Embedding authorization was declined. No remote data was sent.",
    );
  }
  const scope: RemoteEmbeddingAuthorizationScope =
    decision === "allow_workspace" ? "workspace" : "once";
  return {
    authorization: await backend.grantRemoteEmbedding(plan, scope),
  };
}

async function elicitRemoteEmbeddingAuthorization(
  server: McpServer,
  extra: RequestHandlerExtra<ServerRequest, ServerNotification>,
  options: ZvecGrepMcpServerOptions,
  request: Parameters<McpServer["server"]["elicitInput"]>[0],
): ReturnType<McpServer["server"]["elicitInput"]> {
  return await withProgressHeartbeat(
    extra,
    async () =>
      await server.server.elicitInput(request, {
        signal: extra.signal,
        timeout:
          options.authorizationRequestTimeoutMs ?? LONG_RUNNING_MCP_TIMEOUT_MS,
        onprogress: () => undefined,
        resetTimeoutOnProgress: true,
      }),
    {
      intervalMs:
        options.authorizationHeartbeatMs ?? REMOTE_AUTHORIZATION_HEARTBEAT_MS,
      message: "Waiting for Remote Embedding authorization.",
    },
  );
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
