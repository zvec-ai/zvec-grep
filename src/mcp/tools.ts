import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { ZvecGrepContextResult } from "../index.js";
import {
  normalizeSearchInput,
  type NormalizedSearchInput,
} from "./input-normalization.js";
import {
  zvecGrepIndexInputSchema,
  zvecGrepIndexOutputSchema,
  zvecGrepIndexStatusInputSchema,
  zvecGrepIndexStatusOutputSchema,
  zvecGrepSearchInputSchema,
  zvecGrepSearchOutputSchema,
  zvecGrepRemoteEmbeddingDemoInputSchema,
  zvecGrepRemoteEmbeddingDemoOutputSchema,
  zvecGrepServerStatusInputSchema,
  zvecGrepServerStatusOutputSchema,
  type ZvecGrepIndexInput,
  type ZvecGrepIndexStatusInput,
  type ZvecGrepRemoteEmbeddingDemoInput,
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

export type IndexJobState =
  "queued" | "running" | "succeeded" | "failed" | "cancelled";

export type ZvecGrepIndexResult = {
  root: string;
  jobId: string;
  state: IndexJobState;
  reused: boolean;
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

export type RemoteEmbeddingDemoAuthorizationScope =
  "once" | "session" | "workspace";

export type RemoteEmbeddingDemoAuthorization =
  | "granted_once"
  | "granted_session"
  | "existing_session"
  | "granted_workspace"
  | "existing_workspace";

export type RemoteEmbeddingDemoAuthorizationRequest = {
  scope: RemoteEmbeddingDemoAuthorizationScope;
  existing?: boolean;
};

export type ZvecGrepRemoteEmbeddingDemoResult =
  | {
      state: "authorization_required";
      root: string;
      provider: string;
      model: string;
      grantPath: string;
      filePath: string;
      fileBytes: number;
    }
  | {
      state: "completed";
      root: string;
      authorization: RemoteEmbeddingDemoAuthorization;
      scope: RemoteEmbeddingDemoAuthorizationScope;
      provider: string;
      model: string;
      grantPath?: string;
      filePath: string;
      fileBytes: number;
      queryVectorDimensions: number;
      fileVectorDimensions: number;
    };

export interface ZvecGrepDaemonBackend {
  index(
    input: ZvecGrepIndexInput,
    options?: { authorization?: RemoteEmbeddingOperationPermit },
  ): Promise<ZvecGrepIndexResult>;
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
  remoteEmbeddingDemo(
    input: ZvecGrepRemoteEmbeddingDemoInput,
    options?: { authorization?: RemoteEmbeddingDemoAuthorizationRequest },
  ): Promise<ZvecGrepRemoteEmbeddingDemoResult>;
}

export function createZvecGrepMcpServer(
  backend: ZvecGrepDaemonBackend,
  version: string,
): McpServer {
  const server = new McpServer(
    { name: "zvec-grep", version },
    {
      instructions: [
        "Use zvec-grep for indexed repository search.",
        "Every repository operation requires an absolute root path visible to the daemon.",
        "Call zvec_grep_search first. Use its freshness and indexing fields without a status preflight; call zvec_grep_index_status only for a missing index, failed or cancelled indexing, diagnostics, or explicit progress monitoring.",
        "Call zvec_grep_index only when indexing is requested. Its wait parameter defaults to false; poll zvec_grep_index_status for background progress and set wait: true only when completion is required before continuing.",
        "Call zvec_grep_remote_embedding_demo only when the user asks to experience the Remote Embedding authorization demo.",
      ].join(" "),
    },
  );
  registerZvecGrepTools(server, backend);
  return server;
}

export function registerZvecGrepTools(
  server: McpServer,
  backend: ZvecGrepDaemonBackend,
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
    async (input) => {
      const plan = await backend.planIndexAuthorization?.(input);
      const resolution = plan
        ? await resolveRemoteEmbeddingAuthorization(
            server,
            backend,
            plan,
            remoteEmbeddingSessionGrants,
            plan.reason === "index_create" ? "local_index" : undefined,
          )
        : {};
      const effectiveInput =
        resolution.alternative === "local_index"
          ? { ...input, embedding: "local/embeddinggemma-300m" }
          : input;
      const result = await backend.index(effectiveInput, {
        authorization: resolution.authorization,
      });
      const structuredContent = {
        root: result.root,
        job_id: result.jobId,
        state: result.state,
        reused: result.reused,
      };
      return toolResult(
        `root: ${result.root}\njob_id: ${result.jobId}\nstate: ${result.state}\nreused: ${result.reused}`,
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
    async (input) => {
      const normalized = normalizeSearchInput(input);
      const plan = await backend.planSearchAuthorization?.(normalized);
      const resolution = plan
        ? await resolveRemoteEmbeddingAuthorization(
            server,
            backend,
            plan,
            remoteEmbeddingSessionGrants,
            "local_search",
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
    "zvec_grep_remote_embedding_demo",
    {
      title: "Experience Remote Embedding authorization",
      description:
        "Demo-only tool that keeps Remote Embedding authorization inside zg. It offers once, MCP-session, or Workspace scope before performing real Qwen embeddings for query text and one explicitly selected local text file.",
      inputSchema: zvecGrepRemoteEmbeddingDemoInputSchema.shape,
      outputSchema: zvecGrepRemoteEmbeddingDemoOutputSchema.shape,
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: false,
        openWorldHint: false,
      },
    },
    async (input) => {
      let result = await backend.remoteEmbeddingDemo(input);
      if (result.state === "authorization_required") {
        const sessionGrantKey = JSON.stringify([
          result.root,
          result.provider,
          result.model,
        ]);
        if (remoteEmbeddingSessionGrants.has(sessionGrantKey)) {
          result = await backend.remoteEmbeddingDemo(input, {
            authorization: { scope: "session", existing: true },
          });
        } else {
          const elicitation = await server.server.elicitInput({
            mode: "form",
            message: [
              "zvec-grep Remote Embedding demo requires authorization.",
              "",
              `Workspace: ${result.root}`,
              "Provider: Qwen",
              `Model: ${result.model}`,
              "",
              "Remote Embedding permission covers Query + Index data for the selected scope.",
              "Data sent by this demo call:",
              "- Query text for remote query embedding.",
              `- Workspace file content: ${result.filePath} (${result.fileBytes} bytes) for remote document embedding.`,
              "The file body has not been read by the demo yet.",
              "API charges may apply.",
            ].join("\n"),
            requestedSchema: {
              type: "object",
              properties: {
                decision: {
                  type: "string",
                  title: "Remote Embedding authorization",
                  description:
                    "Choose how long zg may reuse this Remote Embedding permission.",
                  oneOf: [
                    { const: "allow_once", title: "Allow once" },
                    {
                      const: "allow_session",
                      title: "Allow for this session",
                    },
                    {
                      const: "allow_workspace",
                      title: "Allow for this workspace",
                    },
                    { const: "cancel", title: "Cancel" },
                  ],
                  default: "cancel",
                },
              },
              required: ["decision"],
            },
          });
          const decision =
            elicitation.action === "accept"
              ? elicitation.content?.decision
              : undefined;
          if (
            decision !== "allow_once" &&
            decision !== "allow_session" &&
            decision !== "allow_workspace"
          ) {
            const structuredContent = {
              root: result.root,
              state: "declined" as const,
              authorization: "declined" as const,
              provider: result.provider,
              model: result.model,
              grant_path: result.grantPath,
              file_path: result.filePath,
              file_bytes: result.fileBytes,
            };
            return toolResult(
              "Remote Embedding demo cancelled. No query or file content was sent, and no authorization was saved.",
              structuredContent,
            );
          }

          const scope =
            decision === "allow_workspace"
              ? "workspace"
              : decision === "allow_session"
                ? "session"
                : "once";
          if (scope === "session") {
            remoteEmbeddingSessionGrants.add(sessionGrantKey);
          }
          result = await backend.remoteEmbeddingDemo(input, {
            authorization: { scope },
          });
        }
      }

      if (result.state !== "completed") {
        throw new Error("Remote Embedding demo authorization was not saved.");
      }
      const structuredContent = {
        root: result.root,
        state: result.state,
        authorization: result.authorization,
        scope: result.scope,
        provider: result.provider,
        model: result.model,
        grant_path: result.grantPath,
        file_path: result.filePath,
        file_bytes: result.fileBytes,
        query_vector_dimensions: result.queryVectorDimensions,
        file_vector_dimensions: result.fileVectorDimensions,
      };
      return toolResult(
        [
          "Remote Embedding demo completed.",
          `authorization: ${result.authorization}`,
          `scope: ${result.scope}`,
          `provider: ${result.provider}`,
          `model: ${result.model}`,
          `file_path: ${result.filePath}`,
          `file_bytes: ${result.fileBytes}`,
          `query_vector_dimensions: ${result.queryVectorDimensions}`,
          `file_vector_dimensions: ${result.fileVectorDimensions}`,
          ...(result.grantPath ? [`grant_path: ${result.grantPath}`] : []),
        ].join("\n"),
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

async function resolveRemoteEmbeddingAuthorization(
  server: McpServer,
  backend: ZvecGrepDaemonBackend,
  plan: RemoteEmbeddingAuthorizationPlan,
  sessionGrants: Set<string>,
  alternative?: "local_search" | "local_index",
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

  const disclosure = [
    ...(plan.disclosure.queryText
      ? ["- Query text for remote query embedding."]
      : []),
    ...(plan.disclosure.workspaceContent !== "none"
      ? [
          plan.disclosure.workspaceContent === "changed"
            ? "- Changed workspace content for remote vector index update."
            : "- Selected workspace content for remote vector indexing.",
        ]
      : []),
  ];
  const elicitation = await server.server.elicitInput({
    mode: "form",
    message: [
      "zvec-grep Remote Embedding requires authorization.",
      "",
      `Workspace: ${plan.target.workspaceRoots.join(", ")}`,
      `Provider: ${plan.target.provider}`,
      `Model: ${plan.target.model}`,
      `Endpoint: ${plan.target.endpoint}`,
      "",
      "Remote Embedding permission covers Query + Index for the selected scope.",
      "Data sent by this operation:",
      ...disclosure,
      "API charges may apply.",
    ].join("\n"),
    requestedSchema: {
      type: "object",
      properties: {
        decision: {
          type: "string",
          title: "Remote Embedding authorization",
          description:
            "Choose how long zg may reuse this Remote Embedding permission.",
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
  });
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
