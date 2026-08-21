import { rmSync } from "node:fs";
import type { EmbeddingRuntimeConfig } from "./config.js";
import { EngineError } from "./errors.js";
import type { WorkspaceIndexInfo } from "./types.js";
import { readJsonFileSync, writeJsonFileSync } from "./utils/json.js";
import { resolveWorkspaceIndexLayout } from "./storage/index.js";

export const WORKSPACE_MANIFEST_FILE = "manifest.json";
export const CURRENT_MANIFEST_VERSION = 1;

const WORKSPACE_DIRECTORY_MODE = 0o700;
const WORKSPACE_MANIFEST_MODE = 0o600;

export type WorkspaceManifest = WorkspaceIndexInfo & {
  manifestVersion: typeof CURRENT_MANIFEST_VERSION;
  embeddingRuntime: EmbeddingRuntimeConfig;
};

export function workspaceManifestPath(home: string): string {
  return resolveWorkspaceIndexLayout(home).manifestPath;
}

export function readWorkspaceManifest(home: string): WorkspaceManifest | null {
  const path = workspaceManifestPath(home);
  const value = readJsonFileSync<unknown>(path, null);
  if (value === null) {
    return null;
  }

  if (!isWorkspaceManifest(value)) {
    throw new EngineError("Workspace index manifest is invalid", {
      code: "ZVEC_GREP.ENGINE.MANIFEST.INVALID",
      context: `path=${path}`,
    });
  }

  return value;
}

export function writeWorkspaceManifest(
  home: string,
  manifest: WorkspaceManifest,
): void {
  writeJsonFileSync(workspaceManifestPath(home), manifest, {
    directoryMode: WORKSPACE_DIRECTORY_MODE,
    fileMode: WORKSPACE_MANIFEST_MODE,
  });
}

export function deleteWorkspaceManifest(home: string): void {
  rmSync(workspaceManifestPath(home), { force: true });
}

export function workspaceIndexInfoFromManifest(
  manifest: WorkspaceManifest,
): WorkspaceIndexInfo {
  return {
    id: manifest.id,
    name: manifest.name,
    path: manifest.path,
    rootPaths: manifest.rootPaths,
    indexPolicy: manifest.indexPolicy,
    embedding: manifest.embedding,
    indexVersion: manifest.indexVersion,
    createdTime: manifest.createdTime,
    updatedTime: manifest.updatedTime,
  };
}

function isWorkspaceManifest(value: unknown): value is WorkspaceManifest {
  if (!isRecord(value) || value.manifestVersion !== CURRENT_MANIFEST_VERSION) {
    return false;
  }

  return (
    isNonEmptyString(value.id) &&
    isNonEmptyString(value.name) &&
    isNonEmptyString(value.path) &&
    Array.isArray(value.rootPaths) &&
    value.rootPaths.length > 0 &&
    value.rootPaths.every(isRootPath) &&
    (value.indexPolicy === "enabled" || value.indexPolicy === "disabled") &&
    (value.embedding === null || isEmbeddingSchema(value.embedding)) &&
    (value.indexVersion === null || Number.isInteger(value.indexVersion)) &&
    typeof value.createdTime === "number" &&
    Number.isFinite(value.createdTime) &&
    typeof value.updatedTime === "number" &&
    Number.isFinite(value.updatedTime) &&
    isEmbeddingRuntime(value.embeddingRuntime)
  );
}

function isRootPath(value: unknown): boolean {
  return (
    isRecord(value) &&
    isNonEmptyString(value.absolutePath) &&
    typeof value.recursive === "boolean" &&
    isOptionalStringArray(value.include) &&
    isOptionalStringArray(value.exclude) &&
    isOptionalStringArray(value.globs) &&
    isOptionalStringArray(value.insensitiveGlobs) &&
    isOptionalStringArray(value.fileTypes) &&
    isOptionalStringArray(value.excludedFileTypes) &&
    isOptionalBoolean(value.hidden) &&
    isOptionalBoolean(value.noIgnore) &&
    isOptionalStringArray(value.ignoreFiles) &&
    isOptionalNonNegativeInteger(value.maxDepth) &&
    isOptionalNonNegativeInteger(value.maxFileSizeBytes) &&
    isOptionalBoolean(value.follow)
  );
}

function isEmbeddingSchema(value: unknown): boolean {
  return (
    isRecord(value) &&
    isNonEmptyString(value.provider) &&
    isNonEmptyString(value.model) &&
    Number.isInteger(value.dimension) &&
    typeof value.dimension === "number" &&
    value.dimension > 0 &&
    (value.metric === "cosine" ||
      value.metric === "dot" ||
      value.metric === "euclidean")
  );
}

function isEmbeddingRuntime(value: unknown): boolean {
  if (!isRecord(value)) {
    return false;
  }

  return (
    (value.apiKey === undefined || typeof value.apiKey === "string") &&
    (value.endpoint === undefined || typeof value.endpoint === "string") &&
    (value.device === undefined ||
      value.device === "auto" ||
      value.device === "cpu" ||
      value.device === "metal" ||
      value.device === "vulkan" ||
      value.device === "cuda")
  );
}

function isOptionalStringArray(value: unknown): boolean {
  return (
    value === undefined ||
    (Array.isArray(value) && value.every((item) => typeof item === "string"))
  );
}

function isOptionalBoolean(value: unknown): boolean {
  return value === undefined || typeof value === "boolean";
}

function isOptionalNonNegativeInteger(value: unknown): boolean {
  return value === undefined || (Number.isInteger(value) && Number(value) >= 0);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isNonEmptyString(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}
