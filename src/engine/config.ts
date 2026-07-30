import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { EngineError } from "./errors/index.js";
import { readJsonFileSync, writeJsonFileSync } from "./utils/json.js";
import { acquireReadWriteLock } from "./utils/lock.js";

export type ZvecGrepGlobalDefaults = {
  embedding?: string;
  modelCacheDir?: string;
  device?: "auto" | "cpu" | "metal" | "vulkan" | "cuda";
};

export type ZvecGrepProviderConfig = {
  apiKey?: string;
  endpoint?: string;
};

export type ZvecGrepEmbeddingModelConfig = {
  device?: "auto" | "cpu" | "metal" | "vulkan" | "cuda";
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

export type ZvecGrepExplicitGlobalOptions = ZvecGrepGlobalDefaults & {
  apiKey?: string;
  endpoint?: string;
};

export function resolveEmbeddingRuntimeOptions(
  reference: string | undefined,
  explicit: Pick<ZvecGrepEmbeddingModelConfig, "device">,
  config: ZvecGrepGlobalConfig,
): ZvecGrepEmbeddingModelConfig {
  if (!reference?.startsWith("local/")) return {};
  const model = config.models?.[reference];
  return {
    device:
      explicit.device ??
      model?.device ??
      config.defaults?.device ??
      environmentDevice(),
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

export function updateGlobalConfigFromExplicitOptions(
  options: ZvecGrepExplicitGlobalOptions,
  indexedEmbedding?: string,
  path = globalConfigPath(),
): boolean {
  const modelReference =
    options.embedding ??
    (indexedEmbedding?.includes("/") ? indexedEmbedding : undefined);
  const localModel = modelReference?.startsWith("local/")
    ? modelReference
    : undefined;
  const defaults: ZvecGrepGlobalDefaults = {
    ...(options.embedding !== undefined
      ? { embedding: options.embedding }
      : {}),
    ...(options.modelCacheDir !== undefined
      ? { modelCacheDir: options.modelCacheDir }
      : {}),
    ...(!localModel && options.device !== undefined
      ? { device: options.device }
      : {}),
  };
  const provider =
    providerFromEmbedding(options.embedding) ??
    providerFromEmbedding(indexedEmbedding) ??
    indexedEmbedding;
  const providerConfig: ZvecGrepProviderConfig =
    provider && provider !== "local"
      ? {
          ...(options.apiKey !== undefined ? { apiKey: options.apiKey } : {}),
          ...(options.endpoint !== undefined
            ? { endpoint: options.endpoint }
            : {}),
        }
      : {};
  const update: ZvecGrepGlobalConfigUpdate = {
    ...(Object.keys(defaults).length > 0 ? { defaults } : {}),
    ...(provider && Object.keys(providerConfig).length > 0
      ? { providers: { [provider]: providerConfig } }
      : {}),
    ...(localModel && options.device !== undefined
      ? {
          models: {
            [localModel]: {
              device: options.device,
            },
          },
        }
      : {}),
  };

  if (!update.defaults && !update.providers && !update.models) {
    return false;
  }

  updateGlobalConfig(update, path);
  return true;
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
    if (!reference.startsWith("local/")) {
      throw invalidConfig(
        path,
        `models.${reference} only supports local embedding models`,
      );
    }
    assertKnownFields(modelValue, ["device"], path, `models.${reference}`);
    const device = optionalDevice(
      modelValue.device,
      path,
      `models.${reference}.device`,
    );
    const config: ZvecGrepEmbeddingModelConfig = {
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
  assertKnownFields(
    value,
    ["embedding", "modelCacheDir", "device"],
    path,
    "defaults",
  );

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
  const device = optionalDevice(value.device, path, "defaults.device");
  const defaults: ZvecGrepGlobalDefaults = {
    ...(embedding ? { embedding } : {}),
    ...(modelCacheDir ? { modelCacheDir } : {}),
    ...(device !== undefined ? { device } : {}),
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
    assertKnownFields(
      providerValue,
      ["apiKey", "endpoint"],
      path,
      `providers.${provider}`,
    );

    const apiKey = optionalNonEmptyString(
      providerValue.apiKey,
      path,
      `providers.${provider}.apiKey`,
    );
    const endpoint = optionalNonEmptyString(
      providerValue.endpoint,
      path,
      `providers.${provider}.endpoint`,
    );
    const config: ZvecGrepProviderConfig = {
      ...(apiKey ? { apiKey } : {}),
      ...(endpoint ? { endpoint } : {}),
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

function environmentDevice(): "auto" | "cpu" | "metal" | "vulkan" | "cuda" {
  const normalized = process.env.ZVEC_GREP_DEVICE?.trim().toLowerCase() ?? "";
  if (["auto", "cpu", "metal", "vulkan", "cuda"].includes(normalized))
    return normalized as "auto" | "cpu" | "metal" | "vulkan" | "cuda";
  return "cpu";
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

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
