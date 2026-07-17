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
  zvecGrepRgInputSchema,
  zvecGrepRgOutputSchema,
  zvecGrepSearchInputSchema,
  zvecGrepSearchOutputSchema,
  zvecGrepServerStatusInputSchema,
  zvecGrepServerStatusOutputSchema,
  type ZvecGrepIndexInput,
  type ZvecGrepIndexStatusInput,
  type ZvecGrepRgInput,
  type ZvecGrepSearchIndexing,
} from "./schemas.js";
import {
  contextText,
  simplifyContextResult,
  toolResult,
} from "./result-format.js";

export type IndexJobState =
  "queued" | "running" | "succeeded" | "failed" | "cancelled";

export type ZvecGrepIndexResult = {
  root: string;
  jobId: string;
  state: IndexJobState;
  reused: boolean;
  action?: "index" | "drop";
  dropped?: boolean;
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

export type ZvecGrepRgResult = {
  root: string;
  result: ZvecGrepContextResult;
};

export interface ZvecGrepDaemonBackend {
  index(input: ZvecGrepIndexInput): Promise<ZvecGrepIndexResult>;
  search(input: NormalizedSearchInput): Promise<ZvecGrepSearchResult>;
  indexStatus(
    input: ZvecGrepIndexStatusInput,
  ): Promise<ZvecGrepIndexStatusResult>;
  rg(input: ZvecGrepRgInput): Promise<ZvecGrepRgResult>;
  serverStatus(): Promise<ZvecGrepServerStatusResult>;
}

export const ZVEC_GREP_MCP_INSTRUCTIONS = [
  "Use zvec-grep before raw grep or rg for repository investigation.",
  "Every repository operation requires an absolute root path visible to the daemon.",
  "Use the zvec_grep_* tools directly for repository search, status, indexing, deletion, and exhaustive lexical search.",
  "Call zvec_grep_search first. Use its freshness and indexing fields without a status preflight; call zvec_grep_index_status only for a missing index, failed or cancelled indexing, diagnostics, or explicit progress monitoring.",
  "Use possibly_stale search results immediately when they are sufficient; do not call status merely because a background update is active.",
  "Use zvec_grep_rg for exhaustive local ripgrep when an index is missing and literal or regex search can answer the task, or when the user explicitly requests rg mode.",
  "Do not switch to zvec_grep_rg merely because semantic search or embedding is unavailable unless exhaustive lexical search fits the task.",
  "Apply focused globs, path filters, and file type filters early; exclude dependencies, generated output, caches, build artifacts, fixtures, and logs unless the task concerns them.",
  "Call zvec_grep_index only when persistent indexing or index deletion is explicitly requested. Never silently create, rebuild, or drop an index.",
  "For a new index, use a user-selected embedding or omit it only when a server default model is known; never guess a model.",
  "zvec_grep_index wait defaults to false; poll zvec_grep_index_status for background progress and set wait to true only when completion is required before continuing.",
  "Use zvec_grep_index with drop: true only when index deletion is explicitly requested.",
  "Call zvec_grep_server_status only for daemon diagnostics, not before ordinary searches.",
].join(" ");

export function createZvecGrepMcpServer(
  backend: ZvecGrepDaemonBackend,
  version: string,
): McpServer {
  const server = new McpServer(
    { name: "zvec-grep", version },
    {
      instructions: ZVEC_GREP_MCP_INSTRUCTIONS,
    },
  );
  registerZvecGrepTools(server, backend);
  return server;
}

export function registerZvecGrepTools(
  server: McpServer,
  backend: ZvecGrepDaemonBackend,
): void {
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
    async (input) => {
      const result = await backend.index(input);
      const structuredContent = {
        root: result.root,
        job_id: result.jobId,
        state: result.state,
        reused: result.reused,
        action: result.action,
        dropped: result.dropped,
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
        "Search an existing repository index first for repository investigation. Read freshness and indexing from the response; use zvec_grep_index_status only for missing indexes, failed or cancelled indexing, diagnostics, or explicit progress monitoring.",
      inputSchema: zvecGrepSearchInputSchema.shape,
      outputSchema: zvecGrepSearchOutputSchema.shape,
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    async (input) => {
      const normalized = normalizeSearchInput(input);
      const response = await backend.search(normalized);
      const structuredContent = {
        root: response.root,
        freshness: response.freshness,
        indexing: response.indexing,
        result: simplifyContextResult(
          response.result,
          normalized.maxContentChars,
        ),
      };
      const statusLines = [
        `freshness: ${response.freshness}`,
        ...(response.indexing
          ? [`indexing: ${formatSearchIndexing(response.indexing)}`]
          : []),
      ];
      return toolResult(
        `${statusLines.join("\n")}\n${contextText(response.result, normalized.maxContentChars)}`,
        structuredContent,
      );
    },
  );

  server.registerTool(
    "zvec_grep_rg",
    {
      title: "Search with managed ripgrep",
      description:
        "Run exhaustive managed ripgrep locally without requiring an index. Use it for literal or regex search, an unindexed repository that can be answered lexically, or an explicit rg-mode request; do not switch to rg merely because semantic search is unavailable.",
      inputSchema: zvecGrepRgInputSchema.shape,
      outputSchema: zvecGrepRgOutputSchema.shape,
      annotations: {
        readOnlyHint: true,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: true,
      },
    },
    async (input) => {
      const response = await backend.rg(input);
      const structuredContent = {
        root: response.root,
        result: simplifyContextResult(response.result, input.maxContentChars),
      };
      return toolResult(
        contextText(response.result, input.maxContentChars),
        structuredContent,
      );
    },
  );

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
