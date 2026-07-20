import { randomUUID } from "node:crypto";
import type { IncomingMessage, ServerResponse } from "node:http";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { isInitializeRequest } from "@modelcontextprotocol/sdk/types.js";
import type { ZvecGrepDaemonBackend } from "./tools.js";
import { createZvecGrepMcpServer } from "./tools.js";

type McpSession = {
  id?: string;
  server: ReturnType<typeof createZvecGrepMcpServer>;
  transport: StreamableHTTPServerTransport;
};

export class McpHttpSessionManager {
  private readonly sessions = new Map<string, McpSession>();
  private readonly initializing = new Set<McpSession>();

  constructor(
    private readonly backend: ZvecGrepDaemonBackend,
    private readonly version: string,
  ) {}

  async handlePost(
    request: IncomingMessage,
    response: ServerResponse,
    body: unknown,
  ): Promise<void> {
    const sessionId = requestSessionId(request);
    if (sessionId) {
      const session = this.sessions.get(sessionId);
      if (!session) {
        writeMcpError(response, 404, "Unknown or expired MCP session.");
        return;
      }
      await session.transport.handleRequest(request, response, body);
      return;
    }

    if (!isInitializeRequest(body)) {
      writeMcpError(
        response,
        400,
        "An MCP initialize request is required before other requests.",
      );
      return;
    }

    const holder: { session?: McpSession } = {};
    const transport = new StreamableHTTPServerTransport({
      sessionIdGenerator: () => randomUUID(),
      onsessioninitialized: (id) => {
        const session = holder.session;
        if (!session) return;
        session.id = id;
        this.sessions.set(id, session);
      },
    });
    const server = createZvecGrepMcpServer(this.backend, this.version);
    const session: McpSession = { server, transport };
    holder.session = session;
    this.initializing.add(session);
    transport.onclose = () => this.forget(session);

    try {
      await server.connect(transport);
      await transport.handleRequest(request, response, body);
    } catch (error) {
      if (!session.id) {
        await server.close().catch(() => undefined);
      }
      throw error;
    } finally {
      this.initializing.delete(session);
    }
  }

  async handleSessionRequest(
    request: IncomingMessage,
    response: ServerResponse,
  ): Promise<void> {
    const sessionId = requestSessionId(request);
    if (!sessionId) {
      response.statusCode = request.method === "GET" ? 405 : 400;
      response.end();
      return;
    }
    const session = this.sessions.get(sessionId);
    if (!session) {
      writeMcpError(response, 404, "Unknown or expired MCP session.");
      return;
    }
    await session.transport.handleRequest(request, response);
  }

  async close(): Promise<void> {
    const sessions = new Set([
      ...this.sessions.values(),
      ...this.initializing.values(),
    ]);
    this.sessions.clear();
    this.initializing.clear();
    await Promise.all(
      [...sessions].map((session) =>
        session.server.close().catch(() => undefined),
      ),
    );
  }

  private forget(session: McpSession): void {
    if (session.id && this.sessions.get(session.id) === session) {
      this.sessions.delete(session.id);
    }
    this.initializing.delete(session);
  }
}

function requestSessionId(request: IncomingMessage): string | undefined {
  const value = request.headers["mcp-session-id"];
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function writeMcpError(
  response: ServerResponse,
  status: number,
  message: string,
): void {
  response.statusCode = status;
  response.setHeader("Content-Type", "application/json");
  response.setHeader("Cache-Control", "no-store");
  response.end(
    JSON.stringify({
      jsonrpc: "2.0",
      error: { code: -32000, message },
      id: null,
    }),
  );
}
