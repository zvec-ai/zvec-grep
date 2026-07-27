import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import {
  ElicitRequestSchema,
  type Progress,
} from "@modelcontextprotocol/sdk/types.js";
import { createInterface } from "node:readline/promises";
import { resolveClientToken } from "../daemon/config.js";
import {
  LONG_RUNNING_MCP_TIMEOUT_MS,
  withProgressHeartbeat,
} from "../mcp/progress-heartbeat.js";

type DaemonToolCallOptions = {
  onProgress?: (progress: Progress) => void;
};

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
    const abortController = new AbortController();
    let cancelledByCtrlC = false;
    const onInterrupt = (): void => {
      abortController.abort(new Error("Operation cancelled by user."));
    };
    const token = await resolveClientToken({
      home: this.options.home,
      tokenFile: this.options.tokenFile,
    });
    process.once("SIGINT", onInterrupt);
    const client = new Client(
      { name: "zvec-grep-cli", version: "1.0.0" },
      { capabilities: { elicitation: { form: {} } } },
    );
    client.setRequestHandler(ElicitRequestSchema, async (request, extra) => {
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
            extra,
            async () =>
              await readline.question(`Choose [1-${cancelChoice}]: `, {
                signal: AbortSignal.any([abortController.signal, extra.signal]),
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
    try {
      await client.connect(transport);
      const result = await client.callTool(
        { name, arguments: args },
        undefined,
        {
          signal: abortController.signal,
          timeout: LONG_RUNNING_MCP_TIMEOUT_MS,
          onprogress: callOptions.onProgress ?? (() => undefined),
          resetTimeoutOnProgress: true,
        },
      );
      if (cancelledByCtrlC) {
        throw new Error("Operation cancelled by user.");
      }
      if (result.isError) {
        const text = toolResultText(result.content);
        throw new Error(text ?? `${name} failed`);
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
      if (abortController.signal.aborted || isCtrlCError(error)) {
        throw new Error("Operation cancelled by user.", { cause: error });
      }
      throw error;
    } finally {
      process.off("SIGINT", onInterrupt);
      await client.close().catch(() => undefined);
    }
  }
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
