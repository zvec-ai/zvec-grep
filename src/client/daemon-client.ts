import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { ElicitRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { createInterface } from "node:readline/promises";
import { resolveClientToken } from "../daemon/config.js";

export class DaemonClient {
  constructor(
    private readonly options: {
      serverUrl: string;
      home?: string;
      tokenFile?: string;
      allowRemote?: "once" | "workspace";
    },
  ) {}

  async callTool(
    name: string,
    args: Record<string, unknown>,
  ): Promise<Record<string, unknown>> {
    const token = await resolveClientToken({
      home: this.options.home,
      tokenFile: this.options.tokenFile,
    });
    const client = new Client(
      { name: "zvec-grep-cli", version: "1.0.0" },
      { capabilities: { elicitation: { form: {} } } },
    );
    client.setRequestHandler(ElicitRequestSchema, async (request) => {
      if (this.options.allowRemote) {
        return {
          action: "accept" as const,
          content: {
            decision:
              this.options.allowRemote === "workspace"
                ? "allow_workspace"
                : "allow_once",
          },
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
        : requested.includes("use_local_index")
          ? {
              value: "use_local_index",
              label: "Use a local embedding model",
            }
          : undefined;
      if (localDecision) console.error(`3. ${localDecision.label}`);
      const cancelChoice = localDecision ? 4 : 3;
      console.error(`${cancelChoice}. Cancel`);
      const readline = createInterface({
        input: process.stdin,
        output: process.stderr,
      });
      try {
        const answer = (
          await readline.question(`Choose [1-${cancelChoice}]: `)
        ).trim();
        if (answer === "1") {
          return {
            action: "accept" as const,
            content: { decision: "allow_once" },
          };
        }
        if (answer === "2") {
          return {
            action: "accept" as const,
            content: { decision: "allow_workspace" },
          };
        }
        if (answer === "3" && localDecision) {
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
      new URL(this.options.serverUrl),
      {
        requestInit: {
          headers: token ? { Authorization: `Bearer ${token}` } : {},
        },
      },
    );
    try {
      await client.connect(transport);
      const result = await client.callTool({ name, arguments: args });
      if (result.isError) {
        const text = Array.isArray(result.content)
          ? result.content.find((item) => item.type === "text")?.text
          : undefined;
        throw new Error(text ?? `${name} failed`);
      }
      return (result.structuredContent ?? {}) as Record<string, unknown>;
    } finally {
      await client.close().catch(() => undefined);
    }
  }
}
