import { randomUUID } from "node:crypto";
import type { IncomingMessage, ServerResponse } from "node:http";
import {
  createMcpHandler,
  isInitializeRequest,
  isLegacyRequest,
} from "@modelcontextprotocol/server";
import {
  NodeStreamableHTTPServerTransport,
  toNodeHandler,
  toWebRequest,
} from "@modelcontextprotocol/node";
import type {
  ZvecGrepDaemonBackend,
  ZvecGrepMcpServerOptions,
} from "./tools.js";
import { createZvecGrepMcpServer } from "./tools.js";
import { InMemoryRemoteEmbeddingRequestStateReplayGuard } from "./request-state.js";

const LEGACY_SESSION_IDLE_TTL_MS = 30 * 60 * 1_000;
const MAX_LEGACY_SESSIONS = 256;

export type McpHttpEndpointOptions = {
  legacySessionIdleTtlMs?: number;
  maxLegacySessions?: number;
  modernOnly?: boolean;
};

type McpSession = {
  activeRequests: number;
  id?: string;
  lastAccessedAt: number;
  server: ReturnType<typeof createZvecGrepMcpServer>;
  transport: NodeStreamableHTTPServerTransport;
};

export class McpHttpEndpoint {
  private readonly sessions = new Map<string, McpSession>();
  private readonly initializing = new Set<McpSession>();
  private readonly serverOptions: Readonly<ZvecGrepMcpServerOptions>;
  private readonly modern;
  private readonly modernNodeHandler;
  private readonly legacySessionIdleTtlMs: number;
  private readonly maxLegacySessions: number;
  private readonly modernOnly: boolean;

  constructor(
    private readonly backend: ZvecGrepDaemonBackend,
    private readonly version: string,
    serverOptions: Readonly<ZvecGrepMcpServerOptions> = {},
    endpointOptions: Readonly<McpHttpEndpointOptions> = {},
  ) {
    this.serverOptions = Object.freeze({
      ...serverOptions,
      requestStateReplayGuard:
        serverOptions.requestStateReplayGuard ??
        new InMemoryRemoteEmbeddingRequestStateReplayGuard(),
    });
    this.legacySessionIdleTtlMs =
      endpointOptions.legacySessionIdleTtlMs ?? LEGACY_SESSION_IDLE_TTL_MS;
    this.maxLegacySessions =
      endpointOptions.maxLegacySessions ?? MAX_LEGACY_SESSIONS;
    this.modernOnly = endpointOptions.modernOnly ?? false;
    if (
      !Number.isFinite(this.legacySessionIdleTtlMs) ||
      this.legacySessionIdleTtlMs <= 0 ||
      !Number.isInteger(this.maxLegacySessions) ||
      this.maxLegacySessions <= 0
    ) {
      throw new RangeError("Legacy MCP session limits must be positive.");
    }
    this.modern = createMcpHandler(
      () =>
        createZvecGrepMcpServer(this.backend, this.version, this.serverOptions),
      { legacy: "reject" },
    );
    this.modernNodeHandler = toNodeHandler(this.modern);
  }

  async handlePost(
    request: IncomingMessage,
    response: ServerResponse,
    body: unknown,
  ): Promise<void> {
    if (this.modernOnly) {
      await this.modernNodeHandler(request, response, body);
      return;
    }
    const webRequest = await toWebRequest(request, body);
    if (!(await isLegacyRequest(webRequest, body))) {
      await this.modernNodeHandler(request, response, body);
      return;
    }
    await this.handleLegacyPost(request, response, body);
  }

  async handleSessionRequest(
    request: IncomingMessage,
    response: ServerResponse,
  ): Promise<void> {
    if (this.modernOnly) {
      await this.modernNodeHandler(request, response);
      return;
    }
    this.expireIdleSessions();
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
    await this.withActiveSession(session, () =>
      session.transport.handleRequest(request, response),
    );
  }

  async close(): Promise<void> {
    const sessions = new Set([
      ...this.sessions.values(),
      ...this.initializing.values(),
    ]);
    this.sessions.clear();
    this.initializing.clear();
    await Promise.all([
      this.modern.close(),
      ...[...sessions].map((session) =>
        session.server.close().catch(() => undefined),
      ),
    ]);
  }

  private async handleLegacyPost(
    request: IncomingMessage,
    response: ServerResponse,
    body: unknown,
  ): Promise<void> {
    this.expireIdleSessions();
    const sessionId = requestSessionId(request);
    if (sessionId) {
      const session = this.sessions.get(sessionId);
      if (!session) {
        writeMcpError(response, 404, "Unknown or expired MCP session.");
        return;
      }
      await this.withActiveSession(session, () =>
        session.transport.handleRequest(request, response, body),
      );
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
    if (this.sessions.size + this.initializing.size >= this.maxLegacySessions) {
      writeMcpError(response, 503, "MCP legacy session limit reached.");
      return;
    }

    const holder: { session?: McpSession } = {};
    const transport = new NodeStreamableHTTPServerTransport({
      sessionIdGenerator: () => randomUUID(),
      onsessioninitialized: (id) => {
        const session = holder.session;
        if (!session) return;
        session.id = id;
        session.lastAccessedAt = Date.now();
        this.sessions.set(id, session);
      },
    });
    const server = createZvecGrepMcpServer(
      this.backend,
      this.version,
      this.serverOptions,
    );
    const session: McpSession = {
      activeRequests: 0,
      server,
      transport,
      lastAccessedAt: Date.now(),
    };
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

  private expireIdleSessions(now = Date.now()): void {
    for (const session of this.sessions.values()) {
      if (
        session.activeRequests > 0 ||
        now - session.lastAccessedAt <= this.legacySessionIdleTtlMs
      ) {
        continue;
      }
      this.forget(session);
      void session.server.close().catch(() => undefined);
    }
  }

  private async withActiveSession(
    session: McpSession,
    operation: () => Promise<void>,
  ): Promise<void> {
    session.activeRequests += 1;
    session.lastAccessedAt = Date.now();
    try {
      await operation();
    } finally {
      session.activeRequests -= 1;
      session.lastAccessedAt = Date.now();
    }
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
