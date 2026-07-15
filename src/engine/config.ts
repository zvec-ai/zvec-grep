import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { EngineError } from "./errors/index.js";
import type { LlamaGpuMode } from "./models/types.js";
import { readJsonFileSync, writeJsonFileSync } from "./utils/json.js";
import { acquireReadWriteLock } from "./utils/lock.js";

export type ZvecGrepGlobalDefaults = {
  embedding?: string;
  modelCacheDir?: string;
  llamaGpu?: LlamaGpuMode;
  embeddingParallelism?: number;
};

export type ZvecGrepProviderConfig = {
  apiKey?: string;
  endpoint?: string;
};

export type ZvecGrepGlobalConfig = {
  version: 1;
  defaults?: ZvecGrepGlobalDefaults;
  providers?: Record<string, ZvecGrepProviderConfig>;
};

export type ZvecGrepGlobalConfigUpdate = {
  defaults?: ZvecGrepGlobalDefaults;
  providers?: Record<string, ZvecGrepProviderConfig>;
};

export type ZvecGrepExplicitGlobalOptions = ZvecGrepGlobalDefaults & {
  apiKey?: string;
  endpoint?: string;
};

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
  indexedProvider?: string,
  path = globalConfigPath(),
): boolean {
  const defaults: ZvecGrepGlobalDefaults = {
    ...(options.embedding !== undefined
      ? { embedding: options.embedding }
      : {}),
    ...(options.modelCacheDir !== undefined
      ? { modelCacheDir: options.modelCacheDir }
      : {}),
    ...(options.llamaGpu !== undefined ? { llamaGpu: options.llamaGpu } : {}),
    ...(options.embeddingParallelism !== undefined
      ? { embeddingParallelism: options.embeddingParallelism }
      : {}),
  };
  const provider = providerFromEmbedding(options.embedding) ?? indexedProvider;
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
  };

  if (!update.defaults && !update.providers) {
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

  const defaults = parseDefaults(value.defaults, path);
  const providers = parseProviders(value.providers, path);
  return {
    version: GLOBAL_CONFIG_VERSION,
    ...(defaults ? { defaults } : {}),
    ...(providers ? { providers } : {}),
  };
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
  const llamaGpu = optionalLlamaGpu(value.llamaGpu, path);
  const embeddingParallelism = optionalPositiveInteger(
    value.embeddingParallelism,
    path,
    "defaults.embeddingParallelism",
  );
  const defaults: ZvecGrepGlobalDefaults = {
    ...(embedding ? { embedding } : {}),
    ...(modelCacheDir ? { modelCacheDir } : {}),
    ...(llamaGpu !== undefined ? { llamaGpu } : {}),
    ...(embeddingParallelism !== undefined ? { embeddingParallelism } : {}),
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

function optionalLlamaGpu(
  value: unknown,
  path: string,
): LlamaGpuMode | undefined {
  if (value === undefined) {
    return undefined;
  }
  if (
    value === false ||
    value === "auto" ||
    value === "metal" ||
    value === "vulkan" ||
    value === "cuda"
  ) {
    return value;
  }
  throw invalidConfig(
    path,
    "defaults.llamaGpu must be auto, metal, vulkan, cuda, or false",
  );
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
