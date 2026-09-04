import type { CreateZvecGrepOptions } from "../engine/service/types.js";
import { DaemonBackend } from "./backend.js";
import {
  configuredListenAddress,
  configuredWatcherIdleTimeoutMs,
  resolveServerToken,
} from "./config.js";
import { DaemonHttpServer } from "./http-server.js";
import { DaemonInstanceLock } from "./server-controller.js";
import { createDaemonLogger } from "./logger.js";
import {
  MCP_TOOLSET_ENV,
  resolveMcpToolset,
  type McpToolset,
} from "../mcp/toolset.js";
import {
  loadOrCreateMcpRequestStateKey,
  PersistentRemoteEmbeddingRequestStateReplayGuard,
} from "../mcp/request-state.js";

export type RunDaemonOptions = {
  version: string;
  listen?: string;
  token?: string;
  tokenFile?: string;
  home?: string;
  mcpToolset?: McpToolset;
  serviceOptions?: CreateZvecGrepOptions;
};

export async function runDaemonForeground(
  options: RunDaemonOptions,
): Promise<void> {
  const mcpToolset = resolveMcpToolset(
    options.mcpToolset,
    process.env[MCP_TOOLSET_ENV],
  );
  const runtimeIdleTtlMs = configuredWatcherIdleTimeoutMs();
  const listen = configuredListenAddress(options.listen);
  const displayAddress = `http://${displayHost(listen.host)}:${listen.port}/mcp`;
  const instanceLock = await DaemonInstanceLock.acquire(
    options.home,
    displayAddress,
    mcpToolset,
  );
  const logger = createDaemonLogger(options.home);
  let auth;
  try {
    auth = await resolveServerToken({
      token: options.token,
      tokenFile: options.tokenFile,
      home: options.home,
    });
  } catch (error) {
    await instanceLock.release();
    throw error;
  }
  let requestStateKey;
  try {
    requestStateKey = await loadOrCreateMcpRequestStateKey(options.home);
  } catch (error) {
    await instanceLock.release();
    throw error;
  }
  const backend = new DaemonBackend({
    version: options.version,
    serviceOptions: options.serviceOptions,
    runtimeIdleTtlMs,
    logger,
  });
  let requestStop: (() => void) | undefined;
  const httpServer = new DaemonHttpServer({
    ...listen,
    token: auth.token,
    version: options.version,
    mcpToolset,
    backend,
    requestStateKey,
    requestStateReplayGuard:
      new PersistentRemoteEmbeddingRequestStateReplayGuard(options.home),
    logger,
    onShutdown: () => requestStop?.(),
  });
  let address;
  try {
    address = await httpServer.start();
  } catch (error) {
    await backend.close();
    await instanceLock.release();
    throw error;
  }
  const stopped = new Promise<void>((resolve) => {
    let stopping = false;
    const stop = () => {
      if (stopping) return;
      stopping = true;
      void (async () => {
        await backend.close();
        await httpServer.close();
        await instanceLock.release();
        logger.event("server.stopped", { pid: process.pid });
        await logger.flush();
      })().finally(resolve);
    };
    requestStop = stop;
    process.once("SIGINT", stop);
    process.once("SIGTERM", stop);
  });
  try {
    await instanceLock.markReady();
  } catch (error) {
    requestStop?.();
    await stopped;
    throw error;
  }
  logger.event("server.ready", {
    host: listen.host,
    port: address.port,
    pid: process.pid,
    mcp_toolset: mcpToolset,
  });
  console.log(
    `zvec-grep server listening on http://${displayHost(address.address)}:${address.port}/mcp`,
  );
  console.log(`MCP toolset: ${mcpToolset}`);
  if (auth.tokenFile) {
    console.log(`Bearer token file: ${auth.tokenFile}`);
  }

  await stopped;
}

function displayHost(host: string): string {
  return host.includes(":") ? `[${host}]` : host;
}
