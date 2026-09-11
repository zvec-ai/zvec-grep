import { randomBytes, timingSafeEqual } from "node:crypto";
import {
  createServer,
  type IncomingMessage,
  type Server,
  type ServerResponse,
} from "node:http";
import type { AddressInfo } from "node:net";
import type { ZvecGrepDaemonBackend } from "../mcp/tools.js";
import { McpHttpEndpoint } from "../mcp/http-transport.js";
import {
  createRemoteEmbeddingRequestStateCodec,
  InMemoryRemoteEmbeddingRequestStateReplayGuard,
  requestPrincipal,
  type RemoteEmbeddingRequestStateReplayGuard,
} from "../mcp/request-state.js";
import { DEFAULT_MCP_TOOLSET, type McpToolset } from "../mcp/toolset.js";
import {
  runWithTraceContext,
  traceContextFromMcpBody,
} from "../observability/trace-context.js";
import { isLoopbackHost, type ServerListenAddress } from "./config.js";
import { requestId, type DaemonLogger } from "./logger.js";

const MAX_REQUEST_BYTES = 1024 * 1024;

export type DaemonHttpServerOptions = ServerListenAddress & {
  token?: string;
  version: string;
  mcpToolset?: McpToolset;
  backend: ZvecGrepDaemonBackend;
  requestStateKey?: Uint8Array;
  requestStateReplayGuard?: RemoteEmbeddingRequestStateReplayGuard;
  legacySessionIdleTtlMs?: number;
  maxLegacySessions?: number;
  onShutdown?: () => void | Promise<void>;
  logger?: DaemonLogger;
};

export class DaemonHttpServer {
  private server?: Server;
  private readonly requestTraceIds = new Map<string, string>();
  private readonly mcpEndpoint: McpHttpEndpoint;
  private readonly adminMcpEndpoint: McpHttpEndpoint;

  constructor(private readonly options: DaemonHttpServerOptions) {
    if (!isLoopbackHost(options.host)) {
      throw new Error("Daemon HTTP server requires a loopback host.");
    }
    const requestStateCodec = createRemoteEmbeddingRequestStateCodec(
      options.requestStateKey ?? randomBytes(32),
    );
    const requestStateReplayGuard =
      options.requestStateReplayGuard ??
      new InMemoryRemoteEmbeddingRequestStateReplayGuard();
    this.mcpEndpoint = new McpHttpEndpoint(
      options.backend,
      options.version,
      {
        toolset: options.mcpToolset ?? DEFAULT_MCP_TOOLSET,
        requestStateCodec,
        requestStateReplayGuard,
      },
      {
        legacySessionIdleTtlMs: options.legacySessionIdleTtlMs,
        maxLegacySessions: options.maxLegacySessions,
      },
    );
    this.adminMcpEndpoint = new McpHttpEndpoint(
      options.backend,
      options.version,
      {
        toolset: "full",
        acceptEmbeddingEnvironmentMeta: true,
        includeSearchStructuredContent: true,
        requestStateCodec,
        requestStateReplayGuard,
      },
      { modernOnly: true },
    );
  }

  async start(): Promise<AddressInfo> {
    if (this.server) {
      return this.address();
    }
    this.server = createServer((request, response) => {
      const id = requestId();
      const startedAt = Date.now();
      void this.handleRequest(request, response, id)
        .catch((error) => {
          this.options.logger?.event("request.failed", {
            request_id: id,
            trace_id: this.requestTraceIds.get(id),
            error_code: errorCode(error),
          });
          if (!response.headersSent) {
            writeJson(response, 500, {
              jsonrpc: "2.0",
              error: { code: -32603, message: "Internal server error." },
              id: null,
            });
          } else if (!response.writableEnded) {
            response.end();
          }
        })
        .finally(() => {
          const path = safeRequestPath(request.url);
          const level =
            request.method === "GET" &&
            path === "/healthz" &&
            response.statusCode === 200
              ? "debug"
              : "info";
          this.options.logger?.event(
            "request.completed",
            {
              request_id: id,
              trace_id: this.requestTraceIds.get(id),
              method: request.method,
              path,
              status: response.statusCode,
              duration_ms: Date.now() - startedAt,
            },
            level,
          );
          this.requestTraceIds.delete(id);
        });
    });
    try {
      await new Promise<void>((resolve, reject) => {
        this.server!.once("error", reject);
        this.server!.listen(this.options.port, this.options.host, () => {
          this.server!.off("error", reject);
          resolve();
        });
      });
    } catch (error) {
      this.server = undefined;
      throw error;
    }
    return this.address();
  }

  address(): AddressInfo {
    const address = this.server?.address();
    if (!address || typeof address === "string") {
      throw new Error("HTTP server is not listening.");
    }
    return address;
  }

  async close(): Promise<void> {
    const server = this.server;
    this.server = undefined;
    if (!server) {
      return;
    }
    await Promise.all([
      this.mcpEndpoint.close(),
      this.adminMcpEndpoint.close(),
    ]);
    await new Promise<void>((resolve, reject) => {
      server.close((error) => (error ? reject(error) : resolve()));
    });
  }

  private async handleRequest(
    request: IncomingMessage,
    response: ServerResponse,
    id: string,
  ): Promise<void> {
    const url = new URL(
      request.url ?? "/",
      `http://${request.headers.host ?? "localhost"}`,
    );
    if (url.pathname === "/healthz") {
      if (request.method !== "GET") {
        writeJson(response, 405, { error: "method_not_allowed" });
        return;
      }
      writeJson(response, 200, { status: "ok" });
      return;
    }

    if (url.pathname === "/control/shutdown") {
      if (
        !validHost(request.headers.host) ||
        !validRequestToken(request.headers.authorization, this.options.token)
      ) {
        writeJson(response, 401, { error: "unauthorized" });
        return;
      }
      if (request.method !== "POST") {
        writeJson(response, 405, { error: "method_not_allowed" });
        return;
      }
      writeJson(response, 202, { status: "stopping" });
      setImmediate(() => {
        void this.options.onShutdown?.();
      });
      return;
    }

    const mcpEndpoint =
      url.pathname === "/mcp"
        ? this.mcpEndpoint
        : url.pathname === "/mcp/admin"
          ? this.adminMcpEndpoint
          : undefined;
    if (!mcpEndpoint) {
      writeJson(response, 404, { error: "not_found" });
      return;
    }
    if (
      !validHost(request.headers.host) ||
      !validOrigin(request.headers.origin)
    ) {
      writeJson(response, 403, { error: "forbidden_origin" });
      return;
    }
    if (!validRequestToken(request.headers.authorization, this.options.token)) {
      response.setHeader("WWW-Authenticate", "Bearer");
      writeJson(response, 401, { error: "unauthorized" });
      return;
    }
    if (request.method === "GET" || request.method === "DELETE") {
      attachRequestPrincipal(request, this.options.token);
      await mcpEndpoint.handleSessionRequest(request, response);
      return;
    }
    if (request.method !== "POST") {
      writeJson(response, 405, {
        jsonrpc: "2.0",
        error: { code: -32000, message: "Method not allowed." },
        id: null,
      });
      return;
    }

    let body: unknown;
    try {
      body = await readJsonBody(request);
    } catch (error) {
      const tooLarge = error instanceof RequestBodyTooLargeError;
      writeJson(response, tooLarge ? 413 : 400, {
        jsonrpc: "2.0",
        error: {
          code: -32700,
          message: tooLarge ? "Request body too large." : "Invalid JSON.",
        },
        id: null,
      });
      return;
    }
    const traceContext = traceContextFromMcpBody(body);
    if (traceContext) this.requestTraceIds.set(id, traceContext.traceId);
    await runWithTraceContext(traceContext, async () => {
      this.options.logger?.event("mcp.request", {
        request_id: id,
        client_id: request.headers["x-client-id"] as string | undefined,
        tool: toolName(body),
      });
      attachRequestPrincipal(request, this.options.token);
      await mcpEndpoint.handlePost(request, response, body);
    });
  }
}

function attachRequestPrincipal(
  request: IncomingMessage,
  token: string | undefined,
): void {
  (
    request as IncomingMessage & { auth?: ReturnType<typeof requestPrincipal> }
  ).auth = requestPrincipal(token);
}

function validHost(hostHeader: string | undefined): boolean {
  if (!hostHeader) {
    return false;
  }
  try {
    return isLoopbackHost(
      new URL(`http://${hostHeader}`).hostname.replace(/^\[|\]$/g, ""),
    );
  } catch {
    return false;
  }
}

function validOrigin(originHeader: string | undefined): boolean {
  if (!originHeader) {
    return true;
  }
  try {
    const origin = new URL(originHeader);
    return (
      origin.protocol === "http:" &&
      isLoopbackHost(origin.hostname.replace(/^\[|\]$/g, ""))
    );
  } catch {
    return false;
  }
}

function validBearerToken(
  header: string | undefined,
  expected: string,
): boolean {
  if (!header?.startsWith("Bearer ")) {
    return false;
  }
  const actual = Buffer.from(header.slice("Bearer ".length), "utf8");
  const expectedBuffer = Buffer.from(expected, "utf8");
  return (
    actual.length === expectedBuffer.length &&
    timingSafeEqual(actual, expectedBuffer)
  );
}

function validRequestToken(
  header: string | undefined,
  expected: string | undefined,
): boolean {
  return expected === undefined || validBearerToken(header, expected);
}

async function readJsonBody(request: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  let bytes = 0;
  for await (const chunk of request) {
    const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    bytes += buffer.length;
    if (bytes > MAX_REQUEST_BYTES) {
      throw new RequestBodyTooLargeError();
    }
    chunks.push(buffer);
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8"));
}

function writeJson(
  response: ServerResponse,
  status: number,
  body: unknown,
): void {
  response.statusCode = status;
  response.setHeader("Content-Type", "application/json");
  response.setHeader("Cache-Control", "no-store");
  response.end(JSON.stringify(body));
}

class RequestBodyTooLargeError extends Error {}

function safeRequestPath(value: string | undefined): string {
  try {
    return new URL(value ?? "/", "http://localhost").pathname;
  } catch {
    return "invalid";
  }
}

function toolName(body: unknown): string | undefined {
  if (typeof body !== "object" || body === null || Array.isArray(body))
    return undefined;
  const value = body as { method?: unknown; params?: { name?: unknown } };
  return value.method === "tools/call" && typeof value.params?.name === "string"
    ? value.params.name
    : typeof value.method === "string"
      ? value.method
      : undefined;
}

function errorCode(error: unknown): string {
  if (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    typeof error.code === "string"
  ) {
    return error.code;
  }
  return "INTERNAL_ERROR";
}
