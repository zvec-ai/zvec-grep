import { lstatSync, rmSync } from "node:fs";
import { join } from "node:path";
import { EngineError } from "./errors.js";
import { INCOMPLETE_RECOVERY_HINT } from "./utils/recovery-guidance.js";
import type {
  RootPath,
  WorkspaceIndexEmbeddingSchema,
  WorkspaceIndexInfo,
  WorkspaceIndexPolicy,
} from "./types.js";
import {
  isCanonicalRelativePath,
  type CanonicalPathResolver,
} from "./utils/canonical-path.js";
import { readJsonFileSync, writeJsonFileSync } from "./utils/json.js";

export const WORKSPACE_MANIFEST_FILE = "manifest.json";
export const CURRENT_MANIFEST_VERSION = 2;
export const LEGACY_MANIFEST_VERSION = 1;

const WORKSPACE_DIRECTORY_MODE = 0o700;
const WORKSPACE_MANIFEST_MODE = 0o600;

/**
 * Persisted scan root: a canonical workspace-relative path plus selection
 * options. Absolute locations are never persisted; they are resolved against
 * the current workspace root at open time.
 */
export type WorkspaceManifestRootPath = Omit<
  RootPath,
  "absolutePath" | "canonicalPath"
> & {
  path: string;
};

/**
 * Runtime configuration persisted in the manifest. Only the embedding
 * endpoint is portable identity (changing it requires a rebuild). API keys
 * and device selection are host bindings and are never persisted.
 */
export type WorkspaceManifestEmbeddingRuntime = {
  endpoint?: string;
};

export type WorkspaceManifest = {
  manifestVersion: typeof CURRENT_MANIFEST_VERSION;
  id: string;
  name: string;
  rootPaths: readonly WorkspaceManifestRootPath[];
  indexPolicy: WorkspaceIndexPolicy;
  embedding: WorkspaceIndexEmbeddingSchema | null;
  indexVersion: number | null;
  createdTime: number;
  updatedTime: number;
  embeddingRuntime: WorkspaceManifestEmbeddingRuntime;
};

export type WorkspaceManifestLocation = {
  /** Current index home (`<workspace>/.zvec-grep`). */
  home: string;
  /** Current workspace root. */
  root: string;
};

export function workspaceManifestPath(home: string): string {
  return join(home, WORKSPACE_MANIFEST_FILE);
}

export type MarkerEntryStatus = "present" | "absent" | "unverifiable";

/**
 * Non-following inspection of the durable INCOMPLETE marker: any entry at
 * the marker path — file, directory, or symlink, including a dangling one —
 * is blockage. Only ENOENT/ENOTDIR count as absence; any other inspection
 * failure is "unverifiable" and must fail closed, never read as absence.
 */
export function incompleteMarkerEntry(home: string): MarkerEntryStatus {
  try {
    lstatSync(join(home, "INCOMPLETE"));
    return "present";
  } catch (error) {
    if (
      typeof error === "object" &&
      error !== null &&
      "code" in error &&
      ((error as { code: unknown }).code === "ENOENT" ||
        (error as { code: unknown }).code === "ENOTDIR")
    ) {
      return "absent";
    }
    return "unverifiable";
  }
}

/**
 * A durable INCOMPLETE marker blocks readers, writers and discovery even
 * after process death or lock cleanup; recovery is the documented operator
 * action. Every entry path that reads an index home — including raw-manifest
 * readers such as export, import and migration — must run this guard under
 * the source lock before reading anything.
 */
export function assertHomeNotIncomplete(home: string): void {
  const status = incompleteMarkerEntry(home);
  if (status === "present") {
    throw new EngineError(
      "Workspace index destination is incomplete from an interrupted reservation",
      {
        code: "ZVEC_GREP.ENGINE.MANIFEST.INCOMPLETE_DESTINATION",
        context: `home=${home} hint=${INCOMPLETE_RECOVERY_HINT}`,
      },
    );
  }
  if (status === "unverifiable") {
    throw new EngineError(
      "Workspace index incomplete-marker state cannot be inspected; treating the destination as blocked",
      {
        code: "ZVEC_GREP.ENGINE.MANIFEST.INCOMPLETE_DESTINATION",
        context: `home=${home} hint=the INCOMPLETE marker could not be inspected. ${INCOMPLETE_RECOVERY_HINT}`,
      },
    );
  }
}

export function readWorkspaceManifest(home: string): WorkspaceManifest | null {
  assertHomeNotIncomplete(home);
  const path = workspaceManifestPath(home);
  const value = readJsonFileSync<unknown>(path, null);
  if (value === null) {
    return null;
  }

  if (isRecord(value) && value.manifestVersion === LEGACY_MANIFEST_VERSION) {
    throw new EngineError(
      "Workspace index uses the legacy absolute-path format and needs migration. Use zg --migrate-index <source-home> <empty-destination-root>. The destination must not contain an index; see docs/09-portable-indexes.md for replacement at the same root.",
      {
        code: "ZVEC_GREP.ENGINE.MANIFEST.MIGRATION_REQUIRED",
        context: `path=${path}`,
      },
    );
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

/**
 * Validate untrusted portable-manifest data and reconstruct it from the
 * allowlist of supported fields, so nothing else is republished. Credential
 * and device material is rejected by the shared reader contract, and
 * unsupported fields at any nesting level are rejected rather than passed
 * through.
 */
export function parsePortableManifest(
  value: unknown,
  context: string,
): WorkspaceManifest {
  if (isRecord(value) && value.manifestVersion === LEGACY_MANIFEST_VERSION) {
    throw new EngineError(
      "Manifest uses the legacy absolute-path format and needs migration",
      {
        code: "ZVEC_GREP.ENGINE.MANIFEST.MIGRATION_REQUIRED",
        context,
      },
    );
  }
  if (!isWorkspaceManifest(value)) {
    throw new EngineError(
      "Portable workspace index manifest is invalid or carries unsupported data",
      {
        code: "ZVEC_GREP.ENGINE.MANIFEST.INVALID",
        context,
      },
    );
  }

  const ROOT_OPTION_KEYS = new Set([
    "path",
    "recursive",
    "include",
    "exclude",
    "globs",
    "insensitiveGlobs",
    "fileTypes",
    "excludedFileTypes",
    "hidden",
    "noIgnore",
    "ignoreFiles",
    "maxDepth",
    "maxFileSizeBytes",
    "follow",
  ]);
  const rootPaths: WorkspaceManifest["rootPaths"] = value.rootPaths.map(
    (root) => {
      for (const key of Object.keys(root)) {
        if (!ROOT_OPTION_KEYS.has(key)) {
          throw new EngineError(
            "Portable manifest root carries an unsupported field",
            {
              code: "ZVEC_GREP.ENGINE.MANIFEST.INVALID",
              context: `${context} root=${root.path} field=${key}`,
            },
          );
        }
      }
      const { path, ...options } = root;
      return { ...options, path };
    },
  );

  const EMBEDDING_KEYS = new Set(["provider", "model", "dimension", "metric"]);
  const embedding = value.embedding
    ? (() => {
        for (const key of Object.keys(value.embedding)) {
          if (!EMBEDDING_KEYS.has(key)) {
            throw new EngineError(
              "Portable manifest embedding schema carries an unsupported field",
              {
                code: "ZVEC_GREP.ENGINE.MANIFEST.INVALID",
                context: `${context} field=${key}`,
              },
            );
          }
        }
        return { ...value.embedding };
      })()
    : null;

  return {
    manifestVersion: CURRENT_MANIFEST_VERSION,
    id: value.id,
    name: value.name,
    rootPaths,
    indexPolicy: value.indexPolicy,
    embedding,
    indexVersion: value.indexVersion,
    createdTime: value.createdTime,
    updatedTime: value.updatedTime,
    embeddingRuntime: {
      ...(value.embeddingRuntime.endpoint !== undefined
        ? { endpoint: value.embeddingRuntime.endpoint }
        : {}),
    },
  };
}

/**
 * Resolve a persisted manifest into the runtime index info for the current
 * workspace location: the index home, absolute root paths, and CRPs are all
 * derived from the current location, never from serialized absolute paths.
 */
export function workspaceIndexInfoFromManifest(
  manifest: WorkspaceManifest,
  location: WorkspaceManifestLocation,
  resolver: CanonicalPathResolver,
): WorkspaceIndexInfo {
  return {
    id: manifest.id,
    name: manifest.name,
    path: location.home,
    rootPaths: manifest.rootPaths.map((root) => {
      const { path, ...options } = root;
      const resolution = resolver.resolveDetailedSync(path);
      if (resolution.status === "forbidden") {
        throw new EngineError(
          "Workspace index root escapes the workspace through a symlink",
          {
            code: "ZVEC_GREP.ENGINE.MANIFEST.ROOT_ESCAPES_WORKSPACE",
            context: `root=${path} resolved=${resolution.path}`,
          },
        );
      }
      const ignoreFiles = root.ignoreFiles?.map((entry) => {
        const resolved = resolver.resolveDetailedSync(entry);
        if (resolved.status === "forbidden") {
          throw new EngineError(
            "Workspace index ignore file escapes the workspace",
            {
              code: "ZVEC_GREP.ENGINE.MANIFEST.ROOT_ESCAPES_WORKSPACE",
              context: `ignoreFile=${entry} resolved=${resolved.path}`,
            },
          );
        }
        return resolved.status === "ok"
          ? resolved.path
          : join(location.root, entry);
      });
      return {
        ...options,
        ...(ignoreFiles !== undefined ? { ignoreFiles } : {}),
        absolutePath:
          resolution.status === "ok"
            ? resolution.path
            : join(location.root, path),
        canonicalPath: path,
      };
    }),
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
    value.rootFingerprint === undefined &&
    isEmbeddingRuntime(value.embeddingRuntime)
  );
}

function isRootPath(value: unknown): boolean {
  return (
    isRecord(value) &&
    isNonEmptyString(value.path) &&
    isCanonicalRelativePath(value.path) &&
    typeof value.recursive === "boolean" &&
    isOptionalStringArray(value.include) &&
    isOptionalStringArray(value.exclude) &&
    isOptionalStringArray(value.globs) &&
    isOptionalStringArray(value.insensitiveGlobs) &&
    isOptionalStringArray(value.fileTypes) &&
    isOptionalStringArray(value.excludedFileTypes) &&
    isOptionalBoolean(value.hidden) &&
    isOptionalBoolean(value.noIgnore) &&
    isOptionalCanonicalArray(value.ignoreFiles) &&
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

  // Host bindings are rejected, not ignored: a manifest carrying an API key
  // or a device must be treated as invalid rather than silently stripped.
  return (
    value.apiKey === undefined &&
    value.device === undefined &&
    (value.endpoint === undefined || typeof value.endpoint === "string")
  );
}

function isOptionalStringArray(value: unknown): boolean {
  return (
    value === undefined ||
    (Array.isArray(value) && value.every((item) => typeof item === "string"))
  );
}

function isOptionalCanonicalArray(value: unknown): boolean {
  return (
    value === undefined ||
    (Array.isArray(value) &&
      value.every(
        (item) => typeof item === "string" && isCanonicalRelativePath(item),
      ))
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
