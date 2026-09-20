import { readGlobalConfig, type ZvecGrepClientMode } from "../engine/config.js";
import { configuredServerUrl } from "../daemon/config.js";

export function resolveClientMode(
  explicit?: ZvecGrepClientMode,
): ZvecGrepClientMode {
  const configured =
    explicit ??
    parseMode(process.env.ZVEC_GREP_MODE) ??
    readGlobalConfig().client?.mode;
  return configured ?? "auto";
}

export function resolveServerUrl(): string {
  return process.env.ZVEC_GREP_SERVER_URL ?? configuredServerUrl();
}

export async function routeByMode<T>(options: {
  mode: ZvecGrepClientMode;
  direct: () => Promise<T>;
  server: () => Promise<T>;
  serverAvailable: () => Promise<boolean>;
}): Promise<T> {
  if (options.mode === "direct") return options.direct();
  if (options.mode === "server") return options.server();
  return (await options.serverAvailable())
    ? options.server()
    : options.direct();
}

function parseMode(value: string | undefined): ZvecGrepClientMode | undefined {
  if (!value) return undefined;
  if (value === "direct" || value === "server" || value === "auto")
    return value;
  throw new Error("ZVEC_GREP_MODE must be direct, server, or auto");
}
