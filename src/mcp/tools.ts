import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { RequestHandlerExtra } from "@modelcontextprotocol/sdk/shared/protocol.js";
import type {
  ServerNotification,
  ServerRequest,
} from "@modelcontextprotocol/sdk/types.js";
import type { IndexProgress, ZvecGrepContextResult } from "../index.js";
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
  zvecGrepSearchInputSchema,
  zvecGrepSearchOutputSchema,
  zvecGrepServerStatusInputSchema,
  zvecGrepServerStatusOutputSchema,
  type ZvecGrepIndexInput,
  type ZvecGrepIndexDropInput,
  type ZvecGrepIndexStatusInput,
  type ZvecGrepSearchIndexing,
} from "./schemas.js";
import {
  contextText,
  simplifyContextResult,
  toolResult,
} from "./result-format.js";
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

export type IndexJobState =
  "queued" | "running" | "succeeded" | "failed" | "cancelled";

export type ZvecGrepIndexResult = {
  root: string;
  jobId: string;
  state: IndexJobState;
  reused: boolean;
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
      indexed: number;
      pending: number;
      failed: number;
      entities: number;
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
  serverStatus(): Promise<ZvecGrepServerStatusResult>;
}

export type ZvecGrepMcpServerOptions = {
  authorizationHeartbeatMs?: number;
  authorizationRequestTimeoutMs?: number;
};

export function createZvecGrepMcpServer(
  backend: ZvecGrepDaemonBackend,
  version: string,
  options: ZvecGrepMcpServerOptions = {},
): McpServer {
  const server = new McpServer(
    { name: "zvec-grep", version },
    {
      instructions: [
        "Use zvec-grep for indexed repository search.",
        "Every repository operation requires an absolute root path visible to the daemon.",
        "Call zvec_grep_search first. Use its freshness and indexing fields without a status preflight; call zvec_grep_index_status only for a missing index, failed or cancelled indexing, diagnostics, or explicit progress monitoring.",
        "Call zvec_grep_index only when indexing is requested. Its wait parameter defaults to false; poll zvec_grep_index_status for background progress and set wait: true only when completion is required before continuing.",
        "Call zvec_grep_index_drop only when the user explicitly asks to delete a Workspace index.",
      ].join(" "),
    },
  );
  registerZvecGrepTools(server, backend, options);
  return server;
}

export function registerZvecGrepTools(
  server: McpServer,
  backend: ZvecGrepDaemonBackend,
  options: ZvecGrepMcpServerOptions = {},
): void {
  const remoteEmbeddingSessionGrants = new Set<string>();
  server.registerTool(
    "zvec_grep_index",
    {
      title: "Ensure zvec-grep index",
      description:
        "Activate an absolute repository root and create or incrementally update its index.",
      inputSchema: zvecGrepIndexInputSchema.shape,
      outputSchema: zvecGrepIndexOutputSchema.shape,
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: false,
        openWorldHint: false,
      },
    },
    async (input, extra) => {
      const plan = await backend.planIndexAuthorization?.(input);
      const resolution = plan
        ? await resolveRemoteEmbeddingAuthorization(
            server,
            backend,
            plan,
            remoteEmbeddingSessionGrants,
            plan.reason === "index_create" ? "local_index" : undefined,
            extra,
            options,
          )
        : {};
      const effectiveInput =
        resolution.alternative === "local_index"
          ? { ...input, embedding: "local/embeddinggemma-300m" }
          : input;
      const progress = createMcpIndexProgressReporter(extra);
      let result: ZvecGrepIndexResult;
      try {
        result = await backend.index(effectiveInput, {
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
        error: result.error,
      };
      return toolResult(
        [
          `root: ${result.root}`,
          `job_id: ${result.jobId}`,
          `state: ${result.state}`,
          `reused: ${result.reused}`,
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

  server.registerTool(
    "zvec_grep_search",
    {
      title: "Search with zvec-grep",
      description:
        "Search an existing repository index and report freshness plus a compact indexing snapshot when results may be stale.",
      inputSchema: zvecGrepSearchInputSchema.shape,
      outputSchema: zvecGrepSearchOutputSchema.shape,
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
            remoteEmbeddingSessionGrants,
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
      const structuredContent = {
        root: response.root,
        freshness: response.freshness,
        indexing: response.indexing,
        result: simplifyContextResult(
          response.result,
          effectiveSearch.maxContentChars,
        ),
      };
      const statusLines = [
        `freshness: ${response.freshness}`,
        ...(response.indexing
          ? [`indexing: ${formatSearchIndexing(response.indexing)}`]
          : []),
      ];
      return toolResult(
        `${statusLines.join("\n")}\n${contextText(response.result, effectiveSearch.maxContentChars)}`,
        structuredContent,
      );
    },
  );

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

  server.registerTool(
    "zvec_grep_index_status",
    {
      title: "Inspect zvec-grep index status",
      description:
        "Read persisted index status and, when active, the daemon runtime and job status for an absolute root.",
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
  "Choose how long zvec-grep may reuse this permission.";

async function resolveRemoteEmbeddingAuthorization(
  server: McpServer,
  backend: ZvecGrepDaemonBackend,
  plan: RemoteEmbeddingAuthorizationPlan,
  sessionGrants: Set<string>,
  alternative: "local_search" | "local_index" | undefined,
  extra: RequestHandlerExtra<ServerRequest, ServerNotification>,
  options: ZvecGrepMcpServerOptions = {},
): Promise<{
  authorization?: RemoteEmbeddingOperationPermit;
  alternative?: "local_search" | "local_index";
}> {
  const existing = await backend.existingRemoteEmbeddingPermit?.(plan);
  if (existing) return { authorization: existing };
  if (!backend.grantRemoteEmbedding) {
    throw new Error("Remote Embedding authorization is required.");
  }
  if (sessionGrants.has(plan.target.targetFingerprint)) {
    return {
      authorization: await backend.grantRemoteEmbedding(plan, "session"),
    };
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
              { const: "allow_session", title: "Allow for this session" },
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
                : alternative === "local_index"
                  ? [
                      {
                        const: "use_local_index",
                        title: "Use a local embedding model",
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
  if (decision === "use_local_index") {
    return { alternative: "local_index" };
  }
  if (
    decision !== "allow_once" &&
    decision !== "allow_session" &&
    decision !== "allow_workspace"
  ) {
    throw new Error(
      "Remote Embedding authorization was declined. No remote data was sent.",
    );
  }
  const scope: RemoteEmbeddingAuthorizationScope =
    decision === "allow_workspace"
      ? "workspace"
      : decision === "allow_session"
        ? "session"
        : "once";
  if (scope === "session") {
    sessionGrants.add(plan.target.targetFingerprint);
  }
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
          error: result.runtime.error,
        }
      : undefined,
  };
}
