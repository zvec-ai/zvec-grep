import { createHash, randomUUID } from "node:crypto";
import { createReadStream, type Stats } from "node:fs";
import {
  mkdir,
  lstat,
  open,
  readFile,
  rename,
  rm,
  stat,
} from "node:fs/promises";
import type { FileHandle } from "node:fs/promises";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { writeJsonFile } from "../utils/json.js";
import {
  acquireModelArtifactCacheLock,
  type ModelArtifactCacheLock,
} from "./artifact-cache-lock.js";

export type ModelArtifactSourceKind = "huggingface" | "modelscope";

export type ModelArtifactSource = Readonly<{
  kind: ModelArtifactSourceKind;
  repo: string;
  revision: string;
  cacheDirectory: string;
  /** Maps a remote artifact path to a path relative to cacheDirectory. */
  localPaths?: Readonly<Record<string, string>>;
}>;

export type ModelArtifact = Readonly<{
  path: string;
  size: number;
  sha256: string;
}>;

export type ModelArtifactDownloadProgress = Readonly<{
  model: string;
  source: ModelArtifactSourceKind;
  artifact: string;
  downloadedBytes: number;
  totalBytes: number;
}>;

export type ResolvedModelArtifacts = Readonly<{
  source: ModelArtifactSource;
  directory: string;
  /** Absolute local paths keyed by the corresponding remote artifact path. */
  paths: Readonly<Record<string, string>>;
}>;

export type ArtifactDownloadFailureKind =
  | "http"
  | "network"
  | "timeout"
  | "integrity"
  | "filesystem"
  | "aborted"
  | "callback"
  | "invalid-input";

export class ArtifactDownloadError extends Error {
  readonly fallbackAllowed: boolean;

  constructor(
    message: string,
    readonly kind: ArtifactDownloadFailureKind,
    readonly source?: ModelArtifactSourceKind,
    readonly artifact?: string,
    readonly status?: number,
    options?: ErrorOptions,
  ) {
    super(message, options);
    this.name = "ArtifactDownloadError";
    this.fallbackAllowed =
      kind === "network" ||
      kind === "timeout" ||
      kind === "integrity" ||
      (kind === "http" &&
        status !== undefined &&
        (status === 403 ||
          status === 404 ||
          status === 408 ||
          status === 429 ||
          (status >= 500 && status <= 599)));
  }
}

export class ModelArtifactResolutionError extends AggregateError {
  constructor(
    readonly model: string,
    readonly primaryError: unknown,
    readonly fallbackError: unknown,
  ) {
    super(
      [primaryError, fallbackError],
      `Unable to download artifacts for ${model} from Hugging Face or ModelScope: ${primaryError instanceof Error ? primaryError.message : String(primaryError)}; ${fallbackError instanceof Error ? fallbackError.message : String(fallbackError)}`,
      { cause: fallbackError },
    );
    this.name = "ModelArtifactResolutionError";
  }
}

export function isFallbackEligibleArtifactError(error: unknown): boolean {
  return error instanceof ArtifactDownloadError && error.fallbackAllowed;
}

export type ArtifactDownloaderDependencies = Readonly<{
  fetch: typeof globalThis.fetch;
  setTimeout: (
    callback: () => void,
    milliseconds: number,
  ) => ReturnType<typeof setTimeout>;
  clearTimeout: (handle: ReturnType<typeof setTimeout>) => void;
  now: () => number;
}>;

export type ResolveModelArtifactsOptions = Readonly<{
  model: string;
  sources: readonly ModelArtifactSource[];
  artifacts: readonly ModelArtifact[];
  onProgress?: (progress: ModelArtifactDownloadProgress) => void;
  /** The verified missing files for this source; resets progress on fallback. */
  onDownloadPlan?: (artifacts: readonly ModelArtifact[]) => void;
  onFallback?: (message: string) => void;
  dependencies?: Partial<ArtifactDownloaderDependencies>;
  timeouts?: Partial<
    Readonly<{
      responseHeaderMs: number;
      readIdleMs: number;
    }>
  >;
  lock?: Partial<
    Readonly<{
      pollMs: number;
      staleMs: number;
      heartbeatMs: number;
    }>
  >;
}>;

const DEFAULT_RESPONSE_HEADER_TIMEOUT_MS = 10_000;
const DEFAULT_READ_IDLE_TIMEOUT_MS = 30_000;
const DEFAULT_LOCK_POLL_MS = 250;
const DEFAULT_LOCK_STALE_MS = 10 * 60_000;
const DEFAULT_LOCK_HEARTBEAT_MS = 30_000;
const MANIFEST_VERSION = 1;

const defaultDependencies: ArtifactDownloaderDependencies = {
  fetch: (...arguments_) => globalThis.fetch(...arguments_),
  setTimeout: (callback, milliseconds) =>
    globalThis.setTimeout(callback, milliseconds),
  clearTimeout: (handle) => globalThis.clearTimeout(handle),
  now: () => Date.now(),
};

type NormalizedOptions = Readonly<{
  model: string;
  sources: readonly ModelArtifactSource[];
  artifacts: readonly ModelArtifact[];
  onProgress?: (progress: ModelArtifactDownloadProgress) => void;
  /** The verified missing files for this source; resets progress on fallback. */
  onDownloadPlan?: (artifacts: readonly ModelArtifact[]) => void;
  onFallback?: (message: string) => void;
  dependencies: ArtifactDownloaderDependencies;
  responseHeaderMs: number;
  readIdleMs: number;
  lockPollMs: number;
  lockStaleMs: number;
  lockHeartbeatMs: number;
}>;

type SourceManifest = Readonly<{
  fingerprint: string;
  markerPath: string;
  lockPath: string;
  localPaths: Readonly<Record<string, string>>;
}>;

type CompleteMarker = Readonly<{
  version: typeof MANIFEST_VERSION;
  fingerprint: string;
  files: Readonly<
    Record<
      string,
      Readonly<{
        size: number;
        mtimeMs: number;
        ctimeMs: number;
      }>
    >
  >;
}>;

type FileIdentity = Readonly<{
  device: number;
  inode: number;
  mode: number;
  size: number;
  mtimeMs: number;
  ctimeMs: number;
  replaceable: boolean;
}>;

class DeadlineError extends Error {
  constructor(readonly phase: "response headers" | "network read") {
    super(`Timed out waiting for ${phase}`);
    this.name = "DeadlineError";
  }
}

/**
 * Resolves a complete, integrity-checked local artifact snapshot.
 *
 * Cache lookup is performed for every source before any network request. Source
 * order is always Hugging Face followed by ModelScope, independent of the input
 * array order.
 */
export async function resolveModelArtifacts(
  options: ResolveModelArtifactsOptions,
): Promise<ResolvedModelArtifacts> {
  const normalized = normalizeOptions(options);
  const manifests = new Map(
    normalized.sources.map((source) => [
      source.kind,
      createSourceManifest(normalized.model, source, normalized.artifacts),
    ]),
  );

  // Check all caches first. In particular, a complete ModelScope snapshot must
  // avoid a doomed Hugging Face network attempt.
  for (const source of normalized.sources) {
    const manifest = manifests.get(source.kind)!;
    if (
      await validateSnapshot(
        normalized.model,
        source,
        normalized.artifacts,
        manifest,
      )
    ) {
      await writeCompleteMarkerBestEffort(
        source,
        normalized.artifacts,
        manifest,
      );
      return resolvedResult(source, manifest);
    }
  }

  let primaryError: unknown;
  let hasPrimaryError = false;
  let warned = false;
  for (const [sourceIndex, source] of normalized.sources.entries()) {
    const manifest = manifests.get(source.kind)!;
    try {
      await downloadSourceSnapshot(normalized, source, manifest);
      return resolvedResult(source, manifest);
    } catch (error) {
      if (sourceIndex === 0) {
        primaryError = error;
        hasPrimaryError = true;
      }

      const fallback = normalized.sources[sourceIndex + 1];
      if (!fallback || !isFallbackEligibleArtifactError(error)) {
        if (hasPrimaryError && sourceIndex > 0) {
          throw new ModelArtifactResolutionError(
            normalized.model,
            primaryError,
            error,
          );
        }
        throw error;
      }

      if (!warned) {
        warned = true;
        invokeFallbackCallback(
          normalized.onFallback,
          `Hugging Face download failed for ${normalized.model}; falling back to ModelScope.`,
          source,
          error,
        );
      }
    }
  }

  // normalizeOptions guarantees at least one source, so this is unreachable.
  throw new ArtifactDownloadError(
    `No artifact source is available for ${normalized.model}`,
    "invalid-input",
  );
}

export function modelArtifactUrl(
  source: Pick<ModelArtifactSource, "kind" | "repo" | "revision">,
  artifactPath: string,
): string {
  const base =
    source.kind === "huggingface"
      ? "https://huggingface.co"
      : "https://modelscope.cn/models";
  return `${base}/${encodePath(source.repo)}/resolve/${encodeURIComponent(source.revision)}/${encodePath(artifactPath)}`;
}

function normalizeOptions(
  options: ResolveModelArtifactsOptions,
): NormalizedOptions {
  if (options.model.trim().length === 0) {
    throw invalidInput("Model reference must not be empty");
  }
  if (options.artifacts.length === 0) {
    throw invalidInput(
      `At least one artifact is required for ${options.model}`,
    );
  }

  const sourceKinds = new Set<ModelArtifactSourceKind>();
  for (const source of options.sources) {
    if (sourceKinds.has(source.kind)) {
      throw invalidInput(`Duplicate artifact source '${source.kind}'`);
    }
    sourceKinds.add(source.kind);
    validateSource(source);
  }
  if (sourceKinds.size === 0) {
    throw invalidInput(
      `At least one artifact source is required for ${options.model}`,
    );
  }

  const artifactPaths = new Set<string>();
  for (const artifact of options.artifacts) {
    validateRelativePath(artifact.path, "artifact path");
    if (artifactPaths.has(artifact.path)) {
      throw invalidInput(`Duplicate artifact path '${artifact.path}'`);
    }
    artifactPaths.add(artifact.path);
    if (!Number.isSafeInteger(artifact.size) || artifact.size < 0) {
      throw invalidInput(`Invalid size for artifact '${artifact.path}'`);
    }
    if (!/^[a-f\d]{64}$/iu.test(artifact.sha256)) {
      throw invalidInput(`Invalid SHA-256 for artifact '${artifact.path}'`);
    }
  }
  for (const source of options.sources) {
    for (const artifactPath of Object.keys(source.localPaths ?? {})) {
      if (!artifactPaths.has(artifactPath)) {
        throw invalidInput(
          `${source.kind} local path mapping references unknown artifact '${artifactPath}'`,
        );
      }
    }
  }

  const dependencies = { ...defaultDependencies, ...options.dependencies };
  const responseHeaderMs =
    options.timeouts?.responseHeaderMs ?? DEFAULT_RESPONSE_HEADER_TIMEOUT_MS;
  const readIdleMs =
    options.timeouts?.readIdleMs ?? DEFAULT_READ_IDLE_TIMEOUT_MS;
  const lockPollMs = options.lock?.pollMs ?? DEFAULT_LOCK_POLL_MS;
  const lockStaleMs = options.lock?.staleMs ?? DEFAULT_LOCK_STALE_MS;
  const lockHeartbeatMs =
    options.lock?.heartbeatMs ?? DEFAULT_LOCK_HEARTBEAT_MS;
  for (const [name, value] of [
    ["responseHeaderMs", responseHeaderMs],
    ["readIdleMs", readIdleMs],
    ["lockPollMs", lockPollMs],
    ["lockStaleMs", lockStaleMs],
    ["lockHeartbeatMs", lockHeartbeatMs],
  ] as const) {
    if (!Number.isFinite(value) || value <= 0) {
      throw invalidInput(`${name} must be a positive number`);
    }
  }
  if (lockHeartbeatMs >= lockStaleMs) {
    throw invalidInput("lock heartbeat must be shorter than its stale timeout");
  }

  const orderedSources = [...options.sources].sort(
    (left, right) => sourceOrder(left.kind) - sourceOrder(right.kind),
  );
  return {
    model: options.model,
    sources: orderedSources,
    artifacts: options.artifacts.map((artifact) => ({
      ...artifact,
      sha256: artifact.sha256.toLowerCase(),
    })),
    onProgress: options.onProgress,
    onDownloadPlan: options.onDownloadPlan,
    onFallback: options.onFallback,
    dependencies,
    responseHeaderMs,
    readIdleMs,
    lockPollMs,
    lockStaleMs,
    lockHeartbeatMs,
  };
}

function validateSource(source: ModelArtifactSource): void {
  if (source.kind !== "huggingface" && source.kind !== "modelscope") {
    throw invalidInput(`Unknown artifact source '${String(source.kind)}'`);
  }
  if (source.repo.trim().length === 0 || source.revision.trim().length === 0) {
    throw invalidInput(`${source.kind} repo and revision must not be empty`);
  }
  validateRelativePath(source.repo, "repository");
  if (
    source.revision.includes("\0") ||
    source.revision === "." ||
    source.revision === ".."
  ) {
    throw invalidInput(`Invalid ${source.kind} revision`);
  }
  if (
    source.cacheDirectory.trim().length === 0 ||
    source.cacheDirectory.includes("\0")
  ) {
    throw invalidInput(`${source.kind} cache directory must not be empty`);
  }
  const localPaths = new Set<string>();
  for (const [artifact, localPath] of Object.entries(source.localPaths ?? {})) {
    validateRelativePath(artifact, "artifact mapping key");
    validateRelativePath(localPath, "local artifact path");
    if (localPaths.has(localPath)) {
      throw invalidInput(
        `Multiple ${source.kind} artifacts map to '${localPath}'`,
      );
    }
    localPaths.add(localPath);
  }
}

function validateRelativePath(path: string, label: string): void {
  const normalized = path.replaceAll("\\", "/");
  if (
    normalized.length === 0 ||
    normalized.includes("\0") ||
    isAbsolute(path) ||
    normalized.startsWith("/") ||
    normalized
      .split("/")
      .some((segment) => segment === "" || segment === "." || segment === "..")
  ) {
    throw invalidInput(`Invalid ${label} '${path}'`);
  }
}

function invalidInput(message: string): ArtifactDownloadError {
  return new ArtifactDownloadError(message, "invalid-input");
}

function sourceOrder(kind: ModelArtifactSourceKind): number {
  return kind === "huggingface" ? 0 : 1;
}

function createSourceManifest(
  model: string,
  source: ModelArtifactSource,
  artifacts: readonly ModelArtifact[],
): SourceManifest {
  const localPaths: Record<string, string> = {};
  const seenPaths = new Set<string>();
  for (const artifact of artifacts) {
    const localPath = source.localPaths?.[artifact.path] ?? artifact.path;
    validateRelativePath(localPath, "local artifact path");
    if (seenPaths.has(localPath)) {
      throw invalidInput(
        `Multiple ${source.kind} artifacts map to '${localPath}'`,
      );
    }
    seenPaths.add(localPath);
    localPaths[artifact.path] = localPath;
  }

  const serialized = JSON.stringify({
    version: MANIFEST_VERSION,
    model,
    source: {
      kind: source.kind,
      repo: source.repo,
      revision: source.revision,
    },
    artifacts: artifacts.map((artifact) => ({
      path: artifact.path,
      localPath: localPaths[artifact.path],
      size: artifact.size,
      sha256: artifact.sha256.toLowerCase(),
    })),
  });
  const fingerprint = createHash("sha256")
    .update(serialized)
    .digest("hex")
    .slice(0, 24);
  return {
    fingerprint,
    markerPath: join(
      source.cacheDirectory,
      `.zvec-grep-artifacts-${fingerprint}.complete`,
    ),
    lockPath: join(
      source.cacheDirectory,
      `.zvec-grep-artifacts-${fingerprint}.lock`,
    ),
    localPaths,
  };
}

function resolvedResult(
  source: ModelArtifactSource,
  manifest: SourceManifest,
): ResolvedModelArtifacts {
  return {
    source,
    directory: resolve(source.cacheDirectory),
    paths: Object.fromEntries(
      Object.entries(manifest.localPaths).map(([artifact, localPath]) => [
        artifact,
        safeLocalPath(source.cacheDirectory, localPath),
      ]),
    ),
  };
}

async function validateSnapshot(
  model: string,
  source: ModelArtifactSource,
  artifacts: readonly ModelArtifact[],
  manifest: SourceManifest,
): Promise<boolean> {
  if (await hasValidCompleteMarker(source, artifacts, manifest)) {
    return true;
  }
  for (const artifact of artifacts) {
    if (!(await validateArtifact(model, source, artifact, manifest))) {
      return false;
    }
  }
  return true;
}

async function hasValidCompleteMarker(
  source: ModelArtifactSource,
  artifacts: readonly ModelArtifact[],
  manifest: SourceManifest,
): Promise<boolean> {
  try {
    let marker: unknown;
    try {
      marker = JSON.parse(await readFile(manifest.markerPath, "utf8"));
    } catch (error) {
      if (isMissingFileError(error)) {
        return false;
      }
      if (error instanceof SyntaxError) {
        return false;
      }
      throw error;
    }
    if (
      !isRecord(marker) ||
      marker.version !== MANIFEST_VERSION ||
      marker.fingerprint !== manifest.fingerprint ||
      !isRecord(marker.files)
    ) {
      return false;
    }
    for (const artifact of artifacts) {
      const recorded = marker.files[artifact.path];
      if (!isRecord(recorded)) {
        return false;
      }
      const stats = await stat(
        safeLocalPath(
          source.cacheDirectory,
          manifest.localPaths[artifact.path],
        ),
      );
      if (
        !stats.isFile() ||
        stats.size !== artifact.size ||
        stats.size !== recorded.size ||
        stats.mtimeMs !== recorded.mtimeMs ||
        stats.ctimeMs !== recorded.ctimeMs
      ) {
        return false;
      }
    }
    return true;
  } catch (error) {
    if (isMissingFileError(error)) {
      return false;
    }
    throw filesystemError(
      `Unable to inspect ${source.kind} artifact completion marker`,
      source,
      undefined,
      error,
    );
  }
}

async function validateArtifact(
  model: string,
  source: ModelArtifactSource,
  artifact: ModelArtifact,
  manifest: SourceManifest,
): Promise<boolean> {
  const localPath = safeLocalPath(
    source.cacheDirectory,
    manifest.localPaths[artifact.path],
  );
  try {
    const stats = await stat(localPath);
    if (!stats.isFile() || stats.size !== artifact.size) {
      return false;
    }
    const hash = createHash("sha256");
    for await (const chunk of createReadStream(localPath)) {
      hash.update(chunk as Buffer);
    }
    return hash.digest("hex") === artifact.sha256.toLowerCase();
  } catch (error) {
    if (isMissingFileError(error)) {
      return false;
    }
    throw filesystemError(
      `Unable to validate cached artifact '${artifact.path}' for ${model}`,
      source,
      artifact.path,
      error,
    );
  }
}

async function downloadSourceSnapshot(
  options: NormalizedOptions,
  source: ModelArtifactSource,
  manifest: SourceManifest,
): Promise<void> {
  try {
    await mkdir(source.cacheDirectory, { recursive: true });
    const lock = await acquireModelArtifactCacheLock(manifest.lockPath, {
      pollMs: options.lockPollMs,
      staleMs: options.lockStaleMs,
      heartbeatMs: options.lockHeartbeatMs,
      dependencies: options.dependencies,
    });
    try {
      // Another process may have completed the snapshot while we waited.
      if (await hasValidCompleteMarker(source, options.artifacts, manifest)) {
        return;
      }
      const missingArtifacts: ModelArtifact[] = [];
      for (const artifact of options.artifacts) {
        if (
          !(await validateArtifact(options.model, source, artifact, manifest))
        ) {
          missingArtifacts.push(artifact);
        }
      }
      invokeDownloadPlanCallback(options, source, missingArtifacts);
      for (const artifact of missingArtifacts) {
        await downloadArtifact(options, source, artifact, manifest, lock);
      }
      await lock.assertOwned();
      await writeCompleteMarker(source, manifest);
    } finally {
      await lock.release();
    }
  } catch (error) {
    if (error instanceof ArtifactDownloadError) {
      throw error;
    }
    throw filesystemError(
      `Unable to prepare ${source.kind} model cache`,
      source,
      undefined,
      error,
    );
  }
}

async function downloadArtifact(
  options: NormalizedOptions,
  source: ModelArtifactSource,
  artifact: ModelArtifact,
  manifest: SourceManifest,
  lock: ModelArtifactCacheLock,
): Promise<void> {
  const destination = safeLocalPath(
    source.cacheDirectory,
    manifest.localPaths[artifact.path],
  );
  const partialPath = `${destination}.part-${process.pid}-${randomUUID()}`;
  const url = modelArtifactUrl(source, artifact.path);
  let originalDestination: FileIdentity | undefined;
  let file: FileHandle | undefined;
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  const controller = new AbortController();
  try {
    try {
      await mkdir(dirname(destination), { recursive: true });
      originalDestination = await inspectFileIdentity(destination);
      file = await open(partialPath, "wx");
    } catch (error) {
      throw filesystemError(
        `Unable to prepare local artifact '${artifact.path}'`,
        source,
        artifact.path,
        error,
      );
    }

    invokeProgressCallback(options, source, artifact, 0);
    let response: Response;
    try {
      response = await withDeadline(
        Promise.resolve(
          options.dependencies.fetch(url, {
            redirect: "follow",
            signal: controller.signal,
          }),
        ),
        options.responseHeaderMs,
        "response headers",
        controller,
        options.dependencies,
      );
    } catch (error) {
      throw requestError(
        `Unable to request model artifact from ${source.kind}`,
        source,
        artifact.path,
        error,
        controller.signal,
      );
    }

    if (!response.ok) {
      await response.body?.cancel().catch(() => undefined);
      throw new ArtifactDownloadError(
        `HTTP ${response.status} ${response.statusText} while downloading model from ${source.kind}`,
        "http",
        source.kind,
        artifact.path,
        response.status,
      );
    }
    if (!response.body) {
      throw new ArtifactDownloadError(
        `Response body is missing for '${artifact.path}' from ${source.kind}`,
        "network",
        source.kind,
        artifact.path,
      );
    }

    reader = response.body.getReader();
    const hash = createHash("sha256");
    let downloadedBytes = 0;
    while (true) {
      let result: ReadableStreamReadResult<Uint8Array>;
      try {
        result = await withDeadline(
          readUntilData(reader),
          options.readIdleMs,
          "network read",
          controller,
          options.dependencies,
        );
      } catch (error) {
        throw requestError(
          `Model download stream failed from ${source.kind}`,
          source,
          artifact.path,
          error,
          controller.signal,
        );
      }
      if (result.done) {
        break;
      }
      try {
        await writeAll(file, result.value);
      } catch (error) {
        throw filesystemError(
          `Unable to write local artifact '${artifact.path}'`,
          source,
          artifact.path,
          error,
        );
      }
      hash.update(result.value);
      downloadedBytes += result.value.byteLength;
      if (downloadedBytes > artifact.size) {
        throw new ArtifactDownloadError(
          `Integrity check failed for '${artifact.path}' from ${source.kind}: response exceeded the expected ${artifact.size} bytes`,
          "integrity",
          source.kind,
          artifact.path,
        );
      }
      await lock.touch();
      invokeProgressCallback(options, source, artifact, downloadedBytes);
    }

    try {
      await file.sync();
      await file.close();
      file = undefined;
    } catch (error) {
      throw filesystemError(
        `Unable to finish local artifact '${artifact.path}'`,
        source,
        artifact.path,
        error,
      );
    }

    const actualSha256 = hash.digest("hex");
    if (
      downloadedBytes !== artifact.size ||
      actualSha256 !== artifact.sha256.toLowerCase()
    ) {
      throw new ArtifactDownloadError(
        `Integrity check failed for '${artifact.path}' from ${source.kind}: expected ${artifact.size} bytes/${artifact.sha256.toLowerCase()}, received ${downloadedBytes} bytes/${actualSha256}`,
        "integrity",
        source.kind,
        artifact.path,
      );
    }

    await installDownloadedArtifact(
      partialPath,
      destination,
      originalDestination,
      options.model,
      source,
      artifact,
      manifest,
      lock,
    );
  } finally {
    controller.abort();
    await reader?.cancel().catch(() => undefined);
    await file?.close().catch(() => undefined);
    await rm(partialPath, { force: true }).catch(() => undefined);
  }
}

async function writeAll(file: FileHandle, bytes: Uint8Array): Promise<void> {
  let offset = 0;
  while (offset < bytes.byteLength) {
    const { bytesWritten } = await file.write(
      bytes,
      offset,
      bytes.byteLength - offset,
      null,
    );
    if (bytesWritten === 0) {
      throw new Error("File write made no progress");
    }
    offset += bytesWritten;
  }
}

async function readUntilData(
  reader: ReadableStreamDefaultReader<Uint8Array>,
): Promise<ReadableStreamReadResult<Uint8Array>> {
  while (true) {
    const result = await reader.read();
    if (result.done || result.value.byteLength > 0) {
      return result;
    }
  }
}

async function installDownloadedArtifact(
  partialPath: string,
  destination: string,
  originalDestination: FileIdentity | undefined,
  model: string,
  source: ModelArtifactSource,
  artifact: ModelArtifact,
  manifest: SourceManifest,
  lock: ModelArtifactCacheLock,
): Promise<void> {
  try {
    await lock.assertOwned();
    const currentDestination = await inspectFileIdentity(destination);
    if (!sameFileIdentity(originalDestination, currentDestination)) {
      if (await validateArtifact(model, source, artifact, manifest)) {
        return;
      }
      throw new Error(
        "Artifact destination changed concurrently while downloading",
      );
    }

    if (currentDestination && !currentDestination.replaceable) {
      throw new Error(
        `Refusing to replace non-file destination '${destination}'`,
      );
    }
    // All downloader writers use the snapshot lock. POSIX rename replaces the
    // destination atomically. Windows rejects that operation when the existing
    // file is open, so preserve the old entry under a unique name while the
    // verified replacement is installed.
    try {
      await rename(partialPath, destination);
    } catch (error) {
      if (
        process.platform !== "win32" ||
        !currentDestination ||
        !isWindowsRenameReplacementError(error)
      ) {
        throw error;
      }
      await replaceDownloadedArtifactOnWindows(
        partialPath,
        destination,
        currentDestination,
        lock,
      );
    }
  } catch (error) {
    if (error instanceof ArtifactDownloadError) {
      throw error;
    }
    throw filesystemError(
      `Unable to install local artifact '${artifact.path}'`,
      source,
      artifact.path,
      error,
    );
  }
}

async function replaceDownloadedArtifactOnWindows(
  partialPath: string,
  destination: string,
  expectedDestination: FileIdentity,
  lock: ModelArtifactCacheLock,
): Promise<void> {
  await lock.assertOwned();
  const currentDestination = await inspectFileIdentity(destination);
  if (!sameFileIdentity(expectedDestination, currentDestination)) {
    throw new Error(
      "Artifact destination changed concurrently while replacing",
    );
  }

  const displacedPath = `${destination}.replaced-${process.pid}-${randomUUID()}`;
  await rename(destination, displacedPath);
  let replacementInstalled = false;
  try {
    await lock.assertOwned();
    if ((await inspectFileIdentity(destination)) !== undefined) {
      throw new Error(
        "Artifact destination was recreated concurrently while replacing",
      );
    }
    await rename(partialPath, destination);
    replacementInstalled = true;
  } catch (error) {
    if ((await inspectFileIdentity(destination)) === undefined) {
      try {
        await rename(displacedPath, destination);
      } catch (restoreError) {
        throw new AggregateError(
          [error, restoreError],
          "Unable to install or restore the local artifact",
          { cause: restoreError },
        );
      }
    }
    throw error;
  } finally {
    if (replacementInstalled) {
      await rm(displacedPath, { force: true });
    }
  }
}

function isWindowsRenameReplacementError(error: unknown): boolean {
  const code = (error as NodeJS.ErrnoException | undefined)?.code;
  return code === "EACCES" || code === "EEXIST" || code === "EPERM";
}

async function writeCompleteMarker(
  source: ModelArtifactSource,
  manifest: SourceManifest,
): Promise<void> {
  try {
    const files: CompleteMarker["files"] = Object.fromEntries(
      await Promise.all(
        Object.entries(manifest.localPaths).map(
          async ([artifactPath, localPath]) => {
            const stats = await stat(
              safeLocalPath(source.cacheDirectory, localPath),
            );
            if (!stats.isFile()) {
              throw new Error(`Artifact '${artifactPath}' is not a file`);
            }
            return [
              artifactPath,
              {
                size: stats.size,
                mtimeMs: stats.mtimeMs,
                ctimeMs: stats.ctimeMs,
              },
            ] as const;
          },
        ),
      ),
    );
    const marker: CompleteMarker = {
      version: MANIFEST_VERSION,
      fingerprint: manifest.fingerprint,
      files,
    };
    await writeJsonFile(manifest.markerPath, marker);
  } catch (error) {
    throw filesystemError(
      `Unable to record completed ${source.kind} model snapshot`,
      source,
      undefined,
      error,
    );
  }
}

async function writeCompleteMarkerBestEffort(
  source: ModelArtifactSource,
  artifacts: readonly ModelArtifact[],
  manifest: SourceManifest,
): Promise<void> {
  try {
    if (await hasValidCompleteMarker(source, artifacts, manifest)) {
      return;
    }
    await writeCompleteMarker(source, manifest);
  } catch {
    // A complete marker is only a hashing optimization. A valid read-only cache
    // remains usable even when metadata cannot be written beside it.
  }
}

async function inspectFileIdentity(
  path: string,
): Promise<FileIdentity | undefined> {
  try {
    return fileIdentity(await lstat(path));
  } catch (error) {
    if (isMissingFileError(error)) {
      return undefined;
    }
    throw error;
  }
}

function fileIdentity(stats: Stats): FileIdentity {
  return {
    device: stats.dev,
    inode: stats.ino,
    mode: stats.mode,
    size: stats.size,
    mtimeMs: stats.mtimeMs,
    ctimeMs: stats.ctimeMs,
    replaceable: stats.isFile() || stats.isSymbolicLink(),
  };
}

function sameFileIdentity(
  left: FileIdentity | undefined,
  right: FileIdentity | undefined,
): boolean {
  return (
    left === right ||
    (left !== undefined &&
      right !== undefined &&
      left.device === right.device &&
      left.inode === right.inode &&
      left.mode === right.mode &&
      left.size === right.size &&
      left.mtimeMs === right.mtimeMs &&
      left.ctimeMs === right.ctimeMs &&
      left.replaceable === right.replaceable)
  );
}

function invokeDownloadPlanCallback(
  options: NormalizedOptions,
  source: ModelArtifactSource,
  artifacts: readonly ModelArtifact[],
): void {
  try {
    options.onDownloadPlan?.(artifacts);
  } catch (error) {
    throw new ArtifactDownloadError(
      `Model download plan callback failed for ${options.model}`,
      "callback",
      source.kind,
      undefined,
      undefined,
      { cause: error },
    );
  }
}

function invokeProgressCallback(
  options: NormalizedOptions,
  source: ModelArtifactSource,
  artifact: ModelArtifact,
  downloadedBytes: number,
): void {
  try {
    options.onProgress?.({
      model: options.model,
      source: source.kind,
      artifact: artifact.path,
      downloadedBytes,
      totalBytes: artifact.size,
    });
  } catch (error) {
    throw new ArtifactDownloadError(
      `Artifact progress callback failed for ${options.model}`,
      "callback",
      source.kind,
      artifact.path,
      undefined,
      { cause: error },
    );
  }
}

function invokeFallbackCallback(
  callback: ((message: string) => void) | undefined,
  message: string,
  source: ModelArtifactSource,
  cause: unknown,
): void {
  try {
    callback?.(message);
  } catch (error) {
    throw new ArtifactDownloadError(
      "Artifact fallback warning callback failed",
      "callback",
      source.kind,
      undefined,
      undefined,
      { cause: new AggregateError([cause, error]) },
    );
  }
}

async function withDeadline<T>(
  promise: Promise<T>,
  timeoutMs: number,
  phase: DeadlineError["phase"],
  controller: AbortController,
  dependencies: ArtifactDownloaderDependencies,
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_resolve, reject) => {
    timer = dependencies.setTimeout(() => {
      const error = new DeadlineError(phase);
      reject(error);
      controller.abort(error);
    }, timeoutMs);
  });
  try {
    return await Promise.race([promise, timeout]);
  } finally {
    if (timer !== undefined) {
      dependencies.clearTimeout(timer);
    }
  }
}

function requestError(
  message: string,
  source: ModelArtifactSource,
  artifact: string,
  error: unknown,
  signal?: AbortSignal,
): ArtifactDownloadError {
  if (error instanceof ArtifactDownloadError) {
    return error;
  }
  const deadlineError =
    error instanceof DeadlineError
      ? error
      : signal?.reason instanceof DeadlineError
        ? signal.reason
        : undefined;
  if (deadlineError) {
    return new ArtifactDownloadError(
      `${message}: ${deadlineError.message}`,
      "timeout",
      source.kind,
      artifact,
      undefined,
      { cause: deadlineError },
    );
  }
  if (isAbortError(error)) {
    return new ArtifactDownloadError(
      `${message}: request was aborted`,
      "aborted",
      source.kind,
      artifact,
      undefined,
      { cause: error },
    );
  }
  return new ArtifactDownloadError(
    `${message}: ${error instanceof Error ? error.message : String(error)}`,
    "network",
    source.kind,
    artifact,
    undefined,
    { cause: error },
  );
}

function filesystemError(
  message: string,
  source: ModelArtifactSource,
  artifact: string | undefined,
  cause: unknown,
): ArtifactDownloadError {
  if (cause instanceof ArtifactDownloadError) {
    return cause;
  }
  return new ArtifactDownloadError(
    message,
    "filesystem",
    source.kind,
    artifact,
    undefined,
    { cause },
  );
}

function isAbortError(error: unknown): boolean {
  return error instanceof DOMException
    ? error.name === "AbortError"
    : error instanceof Error && error.name === "AbortError";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isMissingFileError(error: unknown): boolean {
  return isErrorCode(error, "ENOENT");
}

function isErrorCode(error: unknown, code: string): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    error.code === code
  );
}

function safeLocalPath(cacheDirectory: string, localPath: string): string {
  const root = resolve(cacheDirectory);
  const candidate = resolve(root, localPath);
  const relativePath = relative(root, candidate);
  if (
    relativePath === ".." ||
    relativePath.startsWith(`..${process.platform === "win32" ? "\\" : "/"}`) ||
    isAbsolute(relativePath)
  ) {
    throw invalidInput(`Local artifact path escapes cache: '${localPath}'`);
  }
  return candidate;
}

function encodePath(path: string): string {
  return path
    .replaceAll("\\", "/")
    .split("/")
    .map((segment) => encodeURIComponent(segment))
    .join("/");
}
