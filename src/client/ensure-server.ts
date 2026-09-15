import { randomBytes, randomUUID } from "node:crypto";
import { link, mkdir, unlink, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import { daemonTokenPath, isLoopbackHost } from "../daemon/config.js";
import { serverStatus, startServer } from "../daemon/server-controller.js";

export const IMPLICIT_SERVER_START_TIMEOUT_MS = 3_000;

type EnsureSearchServerOptions = {
  cliPath: string;
  serverUrl: string;
  home?: string;
  tokenFile?: string;
  modelCacheDir?: string;
  onUnavailable?: () => void;
};

/** Only start a local daemon we own; never launch for an arbitrary endpoint. */
export function implicitListenAddress(serverUrl: string): string | undefined {
  let url: URL;
  try {
    url = new URL(serverUrl);
  } catch {
    return undefined;
  }
  if (
    url.protocol !== "http:" ||
    !isLoopbackHost(url.hostname.replace(/^\[|\]$/g, "")) ||
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    url.pathname !== "/mcp"
  )
    return undefined;
  return `${url.hostname}:${url.port || "80"}`;
}

export async function ensureSearchServer(
  options: EnsureSearchServerOptions,
  dependencies = { serverStatus, startServer, ensureImplicitTokenFile },
): Promise<string | undefined> {
  const listen = implicitListenAddress(options.serverUrl);
  if (!listen) return undefined;
  try {
    const status = await dependencies.serverStatus(options.home);
    if (status.running) {
      // A different endpoint is an explicit configuration conflict, not a
      // reason to hijack or restart the user's existing daemon.
      if (status.serverUrl !== options.serverUrl) return undefined;
      if (status.ready) return status.serverUrl;
    }
    const tokenFile =
      options.tokenFile ??
      process.env.ZVEC_GREP_SERVER_TOKEN_FILE ??
      (process.env.ZVEC_GREP_SERVER_TOKEN || status.running
        ? undefined
        : await dependencies.ensureImplicitTokenFile(options.home));
    const started = await dependencies.startServer({
      cliPath: options.cliPath,
      home: options.home,
      listen,
      tokenFile,
      modelCacheDir: options.modelCacheDir,
      timeoutMs: IMPLICIT_SERVER_START_TIMEOUT_MS,
    });
    return started.ready ? started.serverUrl : undefined;
  } catch {
    // Startup is an optimization; a failure must not prevent local search.
    // Do not print raw configuration/error text, which can contain secrets.
    options.onUnavailable?.();
    return undefined;
  }
}

/** Publish a complete token atomically, without rotating a concurrent starter. */
export async function ensureImplicitTokenFile(home?: string): Promise<string> {
  const path = daemonTokenPath(home);
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  const temporary = `${path}.${randomUUID()}.tmp`;
  try {
    await writeFile(temporary, `${randomBytes(32).toString("hex")}\n`, {
      flag: "wx",
      mode: 0o600,
    });
    try {
      await link(temporary, path);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
    }
  } finally {
    await unlink(temporary).catch(() => undefined);
  }
  return path;
}
