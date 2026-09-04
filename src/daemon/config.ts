import { chmod, readFile } from "node:fs/promises";
import { join } from "node:path";
import { readGlobalConfig } from "../engine/config.js";
import { defaultHome } from "../engine/utils/path.js";
import { DaemonError } from "./errors.js";

export const DEFAULT_SERVER_HOST = "127.0.0.1";
export const DEFAULT_SERVER_PORT = 7_999;
export const DEFAULT_WATCHER_IDLE_TIMEOUT_MS = 4 * 60 * 60_000;
export const WATCHER_IDLE_TIMEOUT_SECONDS_ENV =
  "ZVEC_GREP_WATCHER_IDLE_TIMEOUT_SECONDS";

const MAX_TIMER_DELAY_SECONDS = Math.floor(2_147_483_647 / 1_000);

export type ServerListenAddress = {
  host: string;
  port: number;
};

export function daemonHome(home?: string): string {
  return join(home ?? defaultHome(), "daemon");
}

export function daemonTokenPath(home?: string): string {
  return join(daemonHome(home), "token");
}

export function configuredListenAddress(listen?: string): ServerListenAddress {
  if (listen) return parseListenAddress(listen);
  const configured = readGlobalConfig().server;
  return parseListenAddress(
    `${configured?.host ?? DEFAULT_SERVER_HOST}:${configured?.port ?? DEFAULT_SERVER_PORT}`,
  );
}

export function configuredServerUrl(): string {
  const config = readGlobalConfig();
  if (config.client?.serverUrl) return config.client.serverUrl;
  const listen = configuredListenAddress();
  const host = listen.host.includes(":") ? `[${listen.host}]` : listen.host;
  return `http://${host}:${listen.port}/mcp`;
}

export function configuredWatcherIdleTimeoutMs(
  environment: NodeJS.ProcessEnv = process.env,
): number {
  const configured = environment[WATCHER_IDLE_TIMEOUT_SECONDS_ENV]?.trim();
  if (!configured) return DEFAULT_WATCHER_IDLE_TIMEOUT_MS;
  if (!/^\d+$/.test(configured)) {
    throw invalidWatcherIdleTimeout();
  }
  const seconds = Number(configured);
  if (!Number.isSafeInteger(seconds) || seconds > MAX_TIMER_DELAY_SECONDS) {
    throw invalidWatcherIdleTimeout();
  }
  return seconds * 1_000;
}

export function parseListenAddress(value?: string): ServerListenAddress {
  const listen = value ?? `${DEFAULT_SERVER_HOST}:${DEFAULT_SERVER_PORT}`;
  const separator = listen.lastIndexOf(":");
  if (separator <= 0 || separator === listen.length - 1) {
    throw new DaemonError(
      "INVALID_LISTEN_ADDRESS",
      "listen must use host:port format.",
    );
  }
  const host = listen.slice(0, separator).replace(/^\[|\]$/g, "");
  const port = Number(listen.slice(separator + 1));
  if (!isLoopbackHost(host)) {
    throw new DaemonError(
      "LOOPBACK_REQUIRED",
      "Server MVP only supports loopback listen addresses.",
    );
  }
  if (!Number.isInteger(port) || port < 1 || port > 65_535) {
    throw new DaemonError(
      "INVALID_LISTEN_ADDRESS",
      "listen port must be between 1 and 65535.",
    );
  }
  return { host, port };
}

export function isLoopbackHost(host: string): boolean {
  const normalized = host.toLowerCase();
  return (
    normalized === "127.0.0.1" ||
    normalized === "::1" ||
    normalized === "localhost"
  );
}

export async function resolveServerToken(
  options: {
    token?: string;
    tokenFile?: string;
    home?: string;
  } = {},
): Promise<{ token?: string; tokenFile?: string }> {
  const explicit = options.token ?? process.env.ZVEC_GREP_SERVER_TOKEN;
  if (explicit) {
    validateToken(explicit);
    return { token: explicit };
  }

  const tokenFile =
    options.tokenFile ?? process.env.ZVEC_GREP_SERVER_TOKEN_FILE;
  if (!tokenFile) return {};
  const token = (await readFile(tokenFile, "utf8")).trim();
  validateToken(token);
  await chmod(tokenFile, 0o600);
  return { token, tokenFile };
}

export async function resolveClientToken(
  options: {
    tokenFile?: string;
    home?: string;
  } = {},
): Promise<string | undefined> {
  const explicit = process.env.ZVEC_GREP_SERVER_TOKEN;
  if (explicit) {
    validateToken(explicit);
    return explicit;
  }
  const configuredTokenFile =
    options.tokenFile ?? process.env.ZVEC_GREP_SERVER_TOKEN_FILE;
  const tokenFile = configuredTokenFile ?? daemonTokenPath(options.home);
  try {
    const token = (await readFile(tokenFile, "utf8")).trim();
    validateToken(token);
    return token;
  } catch (error) {
    if (!configuredTokenFile) return undefined;
    throw error;
  }
}

function validateToken(token: string): void {
  if (token.length < 32) {
    throw new DaemonError(
      "INVALID_TOKEN",
      "Server token must contain at least 32 characters.",
    );
  }
}

function invalidWatcherIdleTimeout(): DaemonError {
  return new DaemonError(
    "INVALID_WATCHER_IDLE_TIMEOUT",
    `${WATCHER_IDLE_TIMEOUT_SECONDS_ENV} must be an integer between 0 and ${MAX_TIMER_DELAY_SECONDS}; 0 disables idle watcher eviction.`,
  );
}
