import {
  Client,
  StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client";
import type { Progress } from "@modelcontextprotocol/client";
import { createInterface } from "node:readline/promises";
import { resolveClientToken } from "../daemon/config.js";
import {
  LONG_RUNNING_MCP_TIMEOUT_MS,
  withProgressHeartbeat,
} from "../mcp/progress-heartbeat.js";
import { EMBEDDING_ENVIRONMENT_META_KEY } from "../mcp/request-metadata.js";
import { searchFailureFromMeta } from "../mcp/search-failure.js";

type DaemonToolCallOptions = {
  /** Total network deadline including handshake, schema discovery and progress. */
  timeoutMs?: number;
  /** Cancel only this request; shared daemon work has its own lifetime. */
  signal?: AbortSignal;
  onProgress?: (progress: Progress) => void;
  embeddingEnvironment?: string;
  toolContract?: {
    inputProperties?: readonly string[];
    outputProperties?: readonly string[];
    errorMessage: string;
  };
};

export class DaemonCallTimeoutError extends Error {
  constructor() {
    super("Background server request exceeded its time budget.");
    this.name = "DaemonCallTimeoutError";
  }
}

export class DaemonClient {
  constructor(
    private readonly options: {
      serverUrl: string;
      home?: string;
      tokenFile?: string;
      allowRemote?: boolean;
    },
  ) {}

  async callTool(
    name: string,
    args: Record<string, unknown>,
    callOptions: DaemonToolCallOptions = {},
  ): Promise<Record<string, unknown>> {
    return (await this.invokeTool(
      name,
      args,
      callOptions,
      "structured",
    )) as Record<string, unknown>;
  }

  async callTextTool(
    name: string,
    args: Record<string, unknown>,
    callOptions: DaemonToolCallOptions = {},
  ): Promise<string> {
    return (await this.invokeTool(name, args, callOptions, "text")) as string;
  }

  private async invokeTool(
    name: string,
    args: Record<string, unknown>,
    callOptions: DaemonToolCallOptions,
    resultKind: "structured" | "text",
  ): Promise<Record<string, unknown> | string> {
    if (
      callOptions.timeoutMs !== undefined &&
      (!Number.isFinite(callOptions.timeoutMs) || callOptions.timeoutMs <= 0)
    ) {
      throw new Error(
        "Daemon request timeout must be a positive finite duration.",
      );
    }
    if (callOptions.signal?.aborted) {
      throw new Error("Operation cancelled by user.", {
        cause: callOptions.signal.reason,
      });
    }
    const abortController = new AbortController();
    let cancelledByCtrlC = false;
    const onInterrupt = (): void => {
      abortController.abort(new Error("Operation cancelled by user."));
    };
    const token = await resolveClientToken({
      home: this.options.home,
      tokenFile: this.options.tokenFile,
    });
    if (callOptions.signal?.aborted) {
      throw new Error("Operation cancelled by user.", {
        cause: callOptions.signal.reason,
      });
    }
    const client = new Client(
      { name: "zvec-grep-cli", version: "1.0.0" },
      {
        capabilities: { elicitation: { form: {} } },
        versionNegotiation: { mode: { pin: "2026-07-28" } },
      },
    );
    client.setRequestHandler("elicitation/create", async (request, ctx) => {
      if (this.options.allowRemote) {
        return {
          action: "accept" as const,
          content: { decision: "allow_once" },
        };
      }
      if (!process.stdin.isTTY || !process.stdout.isTTY) {
        return { action: "decline" as const };
      }
      console.error(request.params.message);
      console.error("");
      console.error("1. Allow once");
      console.error("2. Allow for this workspace");
      const requested =
        request.params.mode === "form"
          ? JSON.stringify(request.params.requestedSchema)
          : "";
      const localDecision = requested.includes("use_local_search")
        ? { value: "use_local_search", label: "Use FTS only" }
        : undefined;
      if (localDecision) console.error(`3. ${localDecision.label}`);
      const cancelChoice = localDecision ? 4 : 3;
      console.error(`${cancelChoice}. Cancel`);
      const readline = createInterface({
        input: process.stdin,
        output: process.stderr,
      });
      try {
        let answer: string;
        try {
          answer = await withProgressHeartbeat(
            ctx,
            async () =>
              await readline.question(`Choose [1-${cancelChoice}]: `, {
                signal: AbortSignal.any([
                  abortController.signal,
                  ctx.mcpReq.signal,
                ]),
              }),
            { message: "Waiting for Remote Embedding authorization input." },
          );
        } catch (error) {
          if (isCtrlCError(error)) {
            cancelledByCtrlC = true;
            return { action: "cancel" as const };
          }
          throw error;
        }
        const decision = answer.trim();
        if (decision === "1") {
          return {
            action: "accept" as const,
            content: { decision: "allow_once" },
          };
        }
        if (decision === "2") {
          return {
            action: "accept" as const,
            content: { decision: "allow_workspace" },
          };
        }
        if (decision === "3" && localDecision) {
          return {
            action: "accept" as const,
            content: { decision: localDecision.value },
          };
        }
        return { action: "decline" as const };
      } finally {
        readline.close();
      }
    });
    const transport = new StreamableHTTPClientTransport(
      daemonAdminServerUrl(this.options.serverUrl),
      {
        requestInit: {
          headers: token ? { Authorization: `Bearer ${token}` } : {},
        },
      },
    );
    let deadlineError: DaemonCallTimeoutError | undefined;
    const closeOnAbort = (): void => {
      void transport.close().catch(() => undefined);
    };
    const onExternalAbort = (): void => {
      abortController.abort(callOptions.signal?.reason);
    };
    abortController.signal.addEventListener("abort", closeOnAbort, {
      once: true,
    });
    callOptions.signal?.addEventListener("abort", onExternalAbort, {
      once: true,
    });
    process.once("SIGINT", onInterrupt);
    const deadlineTimer =
      callOptions.timeoutMs === undefined
        ? undefined
        : setTimeout(() => {
            if (abortController.signal.aborted) return;
            deadlineError = new DaemonCallTimeoutError();
            abortController.abort(deadlineError);
          }, callOptions.timeoutMs);
    try {
      await client.connect(transport, {
        signal: abortController.signal,
        timeout: callOptions.timeoutMs,
      });
      if (callOptions.toolContract) {
        const listed = await client.listTools(undefined, {
          signal: abortController.signal,
          timeout: callOptions.timeoutMs,
        });
        const tool = listed.tools.find((candidate) => candidate.name === name);
        if (!toolSatisfiesContract(tool, callOptions.toolContract)) {
          throw new Error(callOptions.toolContract.errorMessage);
        }
      }
      const result = await client.callTool(
        {
          name,
          arguments: args,
          ...(callOptions.embeddingEnvironment
            ? {
                _meta: {
                  [EMBEDDING_ENVIRONMENT_META_KEY]:
                    callOptions.embeddingEnvironment,
                },
              }
            : {}),
        },
        {
          signal: abortController.signal,
          timeout: callOptions.timeoutMs ?? LONG_RUNNING_MCP_TIMEOUT_MS,
          onprogress: callOptions.onProgress ?? (() => undefined),
          resetTimeoutOnProgress: callOptions.timeoutMs === undefined,
        },
      );
      if (cancelledByCtrlC) {
        throw new Error("Operation cancelled by user.");
      }
      abortController.signal.throwIfAborted();
      if (result.isError) {
        const text = toolResultText(result.content);
        const message = text ?? `${name} failed`;
        throw (
          searchFailureFromMeta(message, result._meta) ?? new Error(message)
        );
      }
      if (resultKind === "text") {
        const text = toolResultText(result.content);
        if (text === undefined) {
          throw new Error(`${name} returned no text content`);
        }
        return text;
      }
      return (result.structuredContent ?? {}) as Record<string, unknown>;
    } catch (error) {
      if (deadlineError) throw deadlineError;
      if (abortController.signal.aborted || isCtrlCError(error)) {
        throw new Error("Operation cancelled by user.", { cause: error });
      }
      throw error;
    } finally {
      if (deadlineTimer) clearTimeout(deadlineTimer);
      abortController.signal.removeEventListener("abort", closeOnAbort);
      callOptions.signal?.removeEventListener("abort", onExternalAbort);
      process.off("SIGINT", onInterrupt);
      await client.close().catch(() => undefined);
    }
  }
}

export function toolSatisfiesContract(
  tool: { inputSchema?: unknown; outputSchema?: unknown } | null | undefined,
  contract: {
    inputProperties?: readonly string[];
    outputProperties?: readonly string[];
  },
): boolean {
  if (!tool) return false;
  return (
    hasSchemaProperties(tool.inputSchema, contract.inputProperties) &&
    hasSchemaProperties(tool.outputSchema, contract.outputProperties)
  );
}

function hasSchemaProperties(
  schema: unknown,
  required: readonly string[] | undefined,
): boolean {
  if (!required || required.length === 0) return true;
  if (typeof schema !== "object" || schema === null) return false;
  const properties = (schema as { properties?: unknown }).properties;
  if (typeof properties !== "object" || properties === null) return false;
  return required.every((property) => property in properties);
}

export function daemonAdminServerUrl(serverUrl: string): URL {
  const url = new URL(serverUrl);
  const publicPath = url.pathname.replace(/\/+$/, "");
  if (!publicPath.endsWith("/mcp")) {
    throw new Error(
      `zvec-grep server URL must end with /mcp (received ${url.pathname})`,
    );
  }
  url.pathname = `${publicPath}/admin`;
  return url;
}

function toolResultText(content: unknown): string | undefined {
  if (!Array.isArray(content)) {
    return undefined;
  }
  for (const value of content as unknown[]) {
    if (typeof value !== "object" || value === null) {
      continue;
    }
    const item = value as Record<string, unknown>;
    if (item.type === "text" && typeof item.text === "string") {
      return item.text;
    }
  }
  return undefined;
}

function isCtrlCError(error: unknown): boolean {
  return error instanceof Error && /aborted with Ctrl\+C/i.test(error.message);
}
