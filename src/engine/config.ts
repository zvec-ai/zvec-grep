import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { EngineError } from "./errors/index.js";
import { readJsonFileSync, writeJsonFileSync } from "./utils/json.js";
import { acquireReadWriteLock } from "./utils/lock.js";

export type ZvecGrepGlobalDefaults = {
  embedding?: string;
  modelCacheDir?: string;
};

export type ZvecGrepProviderConfig = {
  apiKey?: string;
};

export type EmbeddingDevice = "auto" | "cpu" | "metal" | "vulkan" | "cuda";

export type ZvecGrepEmbeddingModelConfig = {
  endpoint?: string;
  device?: EmbeddingDevice;
};

export type EmbeddingRuntimeConfig = {
  apiKey?: string;
  endpoint?: string;
  device?: EmbeddingDevice;
};

export type ResolvedEmbeddingRuntimeConfig = {
  apiKey: string;
  endpoint?: string;
  device?: EmbeddingDevice;
};

export type ZvecGrepClientMode = "direct" | "server" | "auto";

export type ZvecGrepClientConfig = {
  mode?: ZvecGrepClientMode;
  serverUrl?: string;
};

export type ZvecGrepServerConfig = {
  host?: string;
  port?: number;
};

export type ZvecGrepGlobalConfig = {
  version: 1;
  defaults?: ZvecGrepGlobalDefaults;
  providers?: Record<string, ZvecGrepProviderConfig>;
  models?: Record<string, ZvecGrepEmbeddingModelConfig>;
  client?: ZvecGrepClientConfig;
  server?: ZvecGrepServerConfig;
};

export type ZvecGrepGlobalConfigUpdate = {
  defaults?: ZvecGrepGlobalDefaults;
  providers?: Record<string, ZvecGrepProviderConfig>;
  models?: Record<string, ZvecGrepEmbeddingModelConfig>;
  client?: ZvecGrepClientConfig;
  server?: ZvecGrepServerConfig;
};

export function resolveEmbeddingRuntimeOptions(
  reference: string,
  explicit: EmbeddingRuntimeConfig,
  workspace: EmbeddingRuntimeConfig,
  config: ZvecGrepGlobalConfig,
  environment: NodeJS.ProcessEnv = process.env,
): ResolvedEmbeddingRuntimeConfig {
  const provider = providerFromEmbedding(reference);
  const local = provider === "local";
  if (local && explicit.endpoint !== undefined) {
    throw invalidRuntime(
      reference,
      "endpoint is only supported for remote embedding models",
    );
  }
  if (!local && explicit.device !== undefined) {
    throw invalidRuntime(
      reference,
      "device is only supported for local embedding models",
    );
  }
  const model = config.models?.[reference];
  const providerConfig = provider ? config.providers?.[provider] : undefined;
  const endpoint = !local
    ? (explicit.endpoint ??
      workspace.endpoint ??
      model?.endpoint ??
      nonEmptyEnvironmentValue(environment.ZVEC_GREP_ENDPOINT))
    : undefined;
  if (endpoint !== undefined && !isHttpEndpoint(endpoint)) {
    throw invalidRuntime(reference, "endpoint must be a valid HTTP(S) URL");
  }
  const device = local
    ? (explicit.device ??
      workspace.device ??
      model?.device ??
      environmentDevice(environment))
    : undefined;
  return {
    apiKey:
      explicit.apiKey ??
      workspace.apiKey ??
      providerConfig?.apiKey ??
      environmentApiKey(provider, environment) ??
      "",
    ...(endpoint !== undefined ? { endpoint } : {}),
    ...(device !== undefined ? { device } : {}),
  };
}

const GLOBAL_CONFIG_VERSION = 1;
const GLOBAL_CONFIG_DIRECTORY_MODE = 0o700;
const GLOBAL_CONFIG_FILE_MODE = 0o600;

export function globalConfigPath(): string {
  return resolve(homedir(), ".zvec-grep", "config.json");
}

export function readGlobalConfig(
  path = globalConfigPath(),
): ZvecGrepGlobalConfig {
  const value = readJsonFileSync<unknown>(path, null);
  if (value === null) {
    return emptyGlobalConfig();
  }

  return parseGlobalConfig(value, path);
}

export function updateGlobalConfig(
  update: ZvecGrepGlobalConfigUpdate,
  path = globalConfigPath(),
): ZvecGrepGlobalConfig {
  const lock = acquireReadWriteLock(
    join(dirname(path), "locks", "config"),
    "write",
    {
      operation: "global-config.update",
    },
  );
  try {
    const current = readGlobalConfig(path);
    const next = parseGlobalConfig(
      {
        version: GLOBAL_CONFIG_VERSION,
        defaults: {
          ...current.defaults,
          ...update.defaults,
        },
        providers: mergeProviderConfigs(current.providers, update.providers),
        models: mergeModelConfigs(current.models, update.models),
        client: { ...current.client, ...update.client },
        server: { ...current.server, ...update.server },
      },
      path,
    );

    writeJsonFileSync(path, next, {
      directoryMode: GLOBAL_CONFIG_DIRECTORY_MODE,
      fileMode: GLOBAL_CONFIG_FILE_MODE,
    });
    return next;
  } finally {
    lock.release();
  }
}

function emptyGlobalConfig(): ZvecGrepGlobalConfig {
  return { version: GLOBAL_CONFIG_VERSION };
}

function providerFromEmbedding(
  embedding: string | undefined,
): string | undefined {
  if (!embedding) {
    return undefined;
  }
  const separator = embedding.indexOf("/");
  return separator > 0 ? embedding.slice(0, separator) : undefined;
}

function parseGlobalConfig(value: unknown, path: string): ZvecGrepGlobalConfig {
  if (!isRecord(value) || value.version !== GLOBAL_CONFIG_VERSION) {
    throw invalidConfig(path, "version must be 1");
  }
  assertKnownFields(
    value,
    ["version", "defaults", "providers", "models", "client", "server"],
    path,
    "config",
  );

  const defaults = parseDefaults(value.defaults, path);
  const providers = parseProviders(value.providers, path);
  const models = parseModels(value.models, path);
  const client = parseClient(value.client, path);
  const server = parseServer(value.server, path);
  return {
    version: GLOBAL_CONFIG_VERSION,
    ...(defaults ? { defaults } : {}),
    ...(providers ? { providers } : {}),
    ...(models ? { models } : {}),
    ...(client ? { client } : {}),
    ...(server ? { server } : {}),
  };
}

function parseModels(
  value: unknown,
  path: string,
): Record<string, ZvecGrepEmbeddingModelConfig> | undefined {
  if (value === undefined) return undefined;
  if (!isRecord(value)) throw invalidConfig(path, "models must be an object");
  const models: Record<string, ZvecGrepEmbeddingModelConfig> = {};
  for (const [reference, modelValue] of Object.entries(value)) {
    if (
      !/^[a-z][a-z0-9_-]*\/[A-Za-z0-9._-]+$/.test(reference) ||
      !isRecord(modelValue)
    ) {
      throw invalidConfig(
        path,
        `models.${reference} must be an object with a valid embedding reference`,
      );
    }
    assertKnownFields(
      modelValue,
      ["endpoint", "device"],
      path,
      `models.${reference}`,
    );
    const endpoint = optionalNonEmptyString(
      modelValue.endpoint,
      path,
      `models.${reference}.endpoint`,
    );
    if (endpoint !== undefined && !isHttpEndpoint(endpoint)) {
      throw invalidConfig(
        path,
        `models.${reference}.endpoint must be a valid HTTP(S) URL`,
      );
    }
    const device = optionalDevice(
      modelValue.device,
      path,
      `models.${reference}.device`,
    );
    if (reference.startsWith("local/") && endpoint !== undefined) {
      throw invalidConfig(
        path,
        `models.${reference}.endpoint is only supported for remote models`,
      );
    }
    if (!reference.startsWith("local/") && device !== undefined) {
      throw invalidConfig(
        path,
        `models.${reference}.device is only supported for local models`,
      );
    }
    const config: ZvecGrepEmbeddingModelConfig = {
      ...(endpoint !== undefined ? { endpoint } : {}),
      ...(device !== undefined ? { device } : {}),
    };
    if (Object.keys(config).length > 0) models[reference] = config;
  }
  return Object.keys(models).length > 0 ? models : undefined;
}

function parseClient(
  value: unknown,
  path: string,
): ZvecGrepClientConfig | undefined {
  if (value === undefined) return undefined;
  if (!isRecord(value)) throw invalidConfig(path, "client must be an object");
  assertKnownFields(value, ["mode", "serverUrl"], path, "client");
  const mode = value.mode;
  if (
    mode !== undefined &&
    mode !== "direct" &&
    mode !== "server" &&
    mode !== "auto"
  ) {
    throw invalidConfig(path, "client.mode must be direct, server, or auto");
  }
  const serverUrl = optionalNonEmptyString(
    value.serverUrl,
    path,
    "client.serverUrl",
  );
  return mode || serverUrl
    ? { ...(mode ? { mode } : {}), ...(serverUrl ? { serverUrl } : {}) }
    : undefined;
}

function parseServer(
  value: unknown,
  path: string,
): ZvecGrepServerConfig | undefined {
  if (value === undefined) return undefined;
  if (!isRecord(value)) throw invalidConfig(path, "server must be an object");
  assertKnownFields(value, ["host", "port"], path, "server");
  const host = optionalNonEmptyString(value.host, path, "server.host");
  const port =
    value.port === undefined
      ? undefined
      : optionalPositiveInteger(value.port, path, "server.port");
  if (port !== undefined && port > 65_535)
    throw invalidConfig(path, "server.port must not exceed 65535");
  return host || port
    ? { ...(host ? { host } : {}), ...(port ? { port } : {}) }
    : undefined;
}

function parseDefaults(
  value: unknown,
  path: string,
): ZvecGrepGlobalDefaults | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (!isRecord(value)) {
    throw invalidConfig(path, "defaults must be an object");
  }
  assertKnownFields(value, ["embedding", "modelCacheDir"], path, "defaults");

  const embedding = optionalNonEmptyString(
    value.embedding,
    path,
    "defaults.embedding",
  );
  const modelCacheDir = optionalNonEmptyString(
    value.modelCacheDir,
    path,
    "defaults.modelCacheDir",
  );
  const defaults: ZvecGrepGlobalDefaults = {
    ...(embedding ? { embedding } : {}),
    ...(modelCacheDir ? { modelCacheDir } : {}),
  };
  return Object.keys(defaults).length > 0 ? defaults : undefined;
}

function parseProviders(
  value: unknown,
  path: string,
): Record<string, ZvecGrepProviderConfig> | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (!isRecord(value)) {
    throw invalidConfig(path, "providers must be an object");
  }

  const providers: Record<string, ZvecGrepProviderConfig> = {};
  for (const [provider, providerValue] of Object.entries(value)) {
    if (!/^[a-z][a-z0-9_-]*$/.test(provider) || !isRecord(providerValue)) {
      throw invalidConfig(
        path,
        `providers.${provider} must be an object with a valid provider name`,
      );
    }
    assertKnownFields(providerValue, ["apiKey"], path, `providers.${provider}`);

    const apiKey = optionalNonEmptyString(
      providerValue.apiKey,
      path,
      `providers.${provider}.apiKey`,
    );
    const config: ZvecGrepProviderConfig = {
      ...(apiKey ? { apiKey } : {}),
    };
    if (Object.keys(config).length > 0) {
      providers[provider] = config;
    }
  }

  return Object.keys(providers).length > 0 ? providers : undefined;
}

function mergeProviderConfigs(
  current: Record<string, ZvecGrepProviderConfig> | undefined,
  update: Record<string, ZvecGrepProviderConfig> | undefined,
): Record<string, ZvecGrepProviderConfig> | undefined {
  if (!current && !update) {
    return undefined;
  }

  const merged = { ...current };
  for (const [provider, config] of Object.entries(update ?? {})) {
    merged[provider] = {
      ...merged[provider],
      ...config,
    };
  }
  return merged;
}

function mergeModelConfigs(
  current: Record<string, ZvecGrepEmbeddingModelConfig> | undefined,
  update: Record<string, ZvecGrepEmbeddingModelConfig> | undefined,
): Record<string, ZvecGrepEmbeddingModelConfig> | undefined {
  if (!current && !update) return undefined;
  const merged = { ...current };
  for (const [reference, config] of Object.entries(update ?? {})) {
    merged[reference] = { ...merged[reference], ...config };
  }
  return merged;
}

function optionalNonEmptyString(
  value: unknown,
  path: string,
  field: string,
): string | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (typeof value !== "string" || value.trim().length === 0) {
    throw invalidConfig(path, `${field} must be a non-empty string`);
  }
  return value.trim();
}

function optionalPositiveInteger(
  value: unknown,
  path: string,
  field: string,
): number | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (!Number.isInteger(value) || (value as number) <= 0) {
    throw invalidConfig(path, `${field} must be a positive integer`);
  }
  return value as number;
}

function optionalDevice(
  value: unknown,
  path: string,
  field: string,
): "auto" | "cpu" | "metal" | "vulkan" | "cuda" | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (
    value === "auto" ||
    value === "cpu" ||
    value === "metal" ||
    value === "vulkan" ||
    value === "cuda"
  ) {
    return value;
  }
  throw invalidConfig(
    path,
    `${field} must be auto, cpu, metal, vulkan, or cuda`,
  );
}

function environmentApiKey(
  provider: string | undefined,
  environment: NodeJS.ProcessEnv,
): string | undefined {
  const values =
    provider === "qwen"
      ? [
          environment.ZVEC_GREP_API_KEY,
          environment.DASHSCOPE_API_KEY,
          environment.QWEN_API_KEY,
        ]
      : [environment.ZVEC_GREP_API_KEY];
  return values.map(nonEmptyEnvironmentValue).find(Boolean);
}

function nonEmptyEnvironmentValue(
  value: string | undefined,
): string | undefined {
  const normalized = value?.trim();
  return normalized ? normalized : undefined;
}

function environmentDevice(environment: NodeJS.ProcessEnv): EmbeddingDevice {
  const normalized = environment.ZVEC_GREP_DEVICE?.trim().toLowerCase() ?? "";
  if (["auto", "cpu", "metal", "vulkan", "cuda"].includes(normalized))
    return normalized as EmbeddingDevice;
  return "auto";
}

function assertKnownFields(
  value: Record<string, unknown>,
  allowed: readonly string[],
  path: string,
  field: string,
): void {
  const allowedFields = new Set(allowed);
  for (const key of Object.keys(value)) {
    if (!allowedFields.has(key)) {
      throw invalidConfig(path, `${field}.${key} is not supported`);
    }
  }
}

function invalidConfig(path: string, detail: string): EngineError {
  return new EngineError("zvec-grep global config is invalid", {
    code: "ZVEC_GREP.ENGINE.CONFIG.INVALID",
    context: `path=${path}\ndetail=${detail}`,
  });
}

function invalidRuntime(reference: string, message: string): EngineError {
  return new EngineError("Embedding runtime configuration is invalid", {
    code: "ZVEC_GREP.ENGINE.CONFIG.INVALID_EMBEDDING_RUNTIME",
    context: `reference=${reference}\ndetail=${message}`,
  });
}

function isHttpEndpoint(value: string): boolean {
  try {
    const url = new URL(value);
    return url.protocol === "http:" || url.protocol === "https:";
  } catch {
    return false;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
