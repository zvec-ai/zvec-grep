import { readFileSync, statSync } from "node:fs";
import { createHash, createHmac, randomBytes, randomUUID } from "node:crypto";
import { basename, dirname, join, relative, resolve } from "node:path";
import {
  globalConfigPath,
  readGlobalConfig,
  resolveEmbeddingRuntimeOptions,
  type EmbeddingRuntimeConfig,
  type ResolvedEmbeddingRuntimeConfig,
  type ZvecGrepGlobalConfig,
} from "../config.js";
import { Collection, isCollectionIndexed } from "../collection/index.js";
import {
  collectionDetail,
  detail,
  EngineError,
  errorDetails,
} from "../errors/index.js";
import {
  createEmbeddingModel,
  EmbeddingPurpose,
  type CreateEmbeddingModelOptions,
  type EmbeddingModel,
  type EmbeddingModelInfo,
} from "../models/index.js";
import type {
  CollectionEmbeddingSchema,
  CollectionInfo,
  Content,
  FileInfo,
  IndexResult,
  RootPath,
  SearchHit,
  SearchPlan,
  SearchPlanResult,
} from "../types.js";
import { CURRENT_INDEX_VERSION } from "../types.js";
import { indexStatusNeedsRefresh } from "../index-status.js";
import {
  collectionInfoFromWorkspaceManifest,
  CURRENT_MANIFEST_VERSION,
  readWorkspaceManifest,
  type WorkspaceManifest,
  writeWorkspaceManifest,
} from "../manifest.js";
import { validateRootPaths } from "../pipeline/indexing/root-paths.js";
import {
  findNearestWorkspace,
  hasWorkspaceIndex,
  resetWorkspaceIndexStorage,
  resolveZvecGrepRoot,
  workspaceHome,
  workspaceIndexLocation,
  type WorkspaceIndexLocation,
} from "./root.js";
import { runRgSearch } from "./lexical.js";
import { defaultHome, normalizePath } from "../utils/path.js";
import {
  acquireReadWriteLock,
  assertNoWriteLock,
  type FileLock,
} from "../utils/lock.js";
import { assertDaemonWriteAllowed } from "../utils/daemon-lease.js";
import { TimingCollector } from "../utils/timing.js";
import type {
  CreateZvecGrepOptions,
  ZvecGrep,
  ZvecGrepContextItem,
  ZvecGrepContextOptions,
  ZvecGrepContextResult,
  ZvecGrepInfoOptions,
  ZvecGrepInfoResult,
  ZvecGrepIndexOptions,
} from "./types.js";
import { enrichLexicalItemsWithStructure } from "./structure-enrichment.js";
import { remoteEmbeddingAuthorizationGuard } from "../../authorization/operation.js";
import { RemoteEmbeddingAuthorizationStore } from "../../authorization/store.js";

const DEFAULT_CONTEXT_LIMIT = 10;
const DEFAULT_CONTEXT_TOTAL_LIMIT = 30;
const DEFAULT_LOCAL_EMBEDDING = "local/embeddinggemma-300m";
const PROVIDER_API_KEY_IDENTITY_SECRET = randomBytes(32);
const MAX_RECOVERED_EMBEDDING_MODELS = 4;

export type EmbeddingModelIdentity = Pick<
  EmbeddingModelInfo,
  "provider" | "name"
>;

export async function createZvecGrep(
  options: CreateZvecGrepOptions = {},
): Promise<ZvecGrep> {
  return new ZvecGrepService(options);
}

export type WorkspaceReadSession = {
  readonly root: string;
  context(options: ZvecGrepContextOptions): Promise<ZvecGrepContextResult>;
  close(): Promise<void>;
};

export function openWorkspaceReadSession(
  startRoot: string,
  embeddingModel?: EmbeddingModel,
): WorkspaceReadSession {
  const start = resolveZvecGrepRoot(startRoot);
  assertNearestWorkspaceHomeUnlocked(start, "daemon.context.open");
  const nearest = findNearestWorkspaceCollection(start);
  if (!nearest) {
    throw workspaceIndexMissingError(start, "undecided");
  }

  const { location, info } = nearest;
  if (info.indexPolicy === "disabled") {
    throw workspaceIndexDisabledError(location.root);
  }
  if (!isCollectionIndexed(info) || !hasWorkspaceIndex(location)) {
    throw workspaceIndexMissingError(
      location.root,
      info.indexPolicy ?? "enabled",
    );
  }

  const collection = new Collection(
    info,
    embeddingModel,
    true,
    location.filesPath,
  );
  let closed = false;

  return {
    root: location.root,
    async context(options) {
      if (closed) {
        throw new EngineError("Workspace read session is already closed", {
          code: "ZVEC_GREP.ENGINE.SERVICE.READ_SESSION_CLOSED",
        });
      }
      const timings = new TimingCollector();
      const request = normalizeContextRequest(options);
      const result = await timings.time("total", () =>
        withHomeReadLock(location.home, "daemon.context", () =>
          contextFromOpenCollection({
            root: location.root,
            request,
            collection,
            options: { ...options, autoUpdate: false },
            timings,
          }),
        ),
      );
      return withContextTimings(result, timings);
    },
    async close() {
      if (closed) {
        return;
      }
      collection.close();
      closed = true;
    },
  };
}

export function createEmbeddingModelForIdentity(
  identity: EmbeddingModelIdentity,
  options: CreateZvecGrepOptions = {},
  workspaceRuntime: EmbeddingRuntimeConfig = {},
): EmbeddingModel {
  const reference = embeddingModelReference(identity);
  return createServiceEmbeddingModel(
    reference,
    providerOptions(options, identity, readGlobalConfig(), workspaceRuntime),
    options,
  );
}

export function embeddingModelPoolKeyForIdentity(
  identity: EmbeddingModelIdentity,
  options: CreateZvecGrepOptions = {},
  workspaceRuntime: EmbeddingRuntimeConfig = {},
): string {
  const reference = embeddingModelReference(identity);
  const fingerprint = providerOptionsFingerprint(
    providerOptions(options, identity, readGlobalConfig(), workspaceRuntime),
  );
  return [reference, fingerprint].join("/");
}

class ZvecGrepService implements ZvecGrep {
  readonly root: string;
  private readonly embeddingModel?: EmbeddingModel;
  private readonly recoveredEmbeddingModels = new Map<string, EmbeddingModel>();
  private readonly retiredEmbeddingModels = new Set<EmbeddingModel>();
  private activeEmbeddingModelOperations = 0;
  private closed = false;

  constructor(private readonly options: CreateZvecGrepOptions) {
    this.root = resolveZvecGrepRoot(options.root);
    this.embeddingModel = options.embeddingModel;
  }

  async index(options: ZvecGrepIndexOptions = {}): Promise<IndexResult> {
    this.ensureOpen();
    const root = resolveZvecGrepRoot(options.root ?? this.root);
    const daemonWritePermit = assertDaemonWriteAllowed(
      root,
      this.options.daemonInstanceToken,
    );
    const location = workspaceIndexLocation(root);
    try {
      return await this.withEmbeddingModelOperation(() =>
        withHomeWriteLock(
          location.home,
          options.rebuild ? "index.rebuild" : "index",
          async () => {
            const existing = readWorkspaceManifest(location.home);
            const existingRuntime = existing?.embeddingRuntime ?? {};
            const embeddingModel = this.embeddingModelForIndex(
              existing,
              "index",
              existingRuntime,
            );
            const effectiveRuntime = effectiveEmbeddingRuntime(
              this.options,
              embeddingModel,
              runtimeForModelProvider(
                existing,
                embeddingModel,
                existingRuntime,
              ),
            );
            if (!options.rebuild) {
              assertCollectionEndpointMatchesCurrentRuntime(
                existing,
                existingRuntime,
                effectiveRuntime,
                "zg index --endpoint <url> --rebuild",
              );
            }
            const rootPaths = resolveIndexRootPaths(
              existing,
              options.rootPaths,
              root,
              {
                resetPaths: options.resetPaths === true,
                includePaths: options.includePaths,
                excludePaths: options.excludePaths,
                globs: options.globs,
                insensitiveGlobs: options.insensitiveGlobs,
                fileTypes: options.fileTypes,
                excludedFileTypes: options.excludedFileTypes,
                hidden: options.hidden,
                noIgnore: options.noIgnore,
                ignoreFiles: options.ignoreFiles,
                maxDepth: options.maxDepth,
                maxFileSizeBytes: options.maxFileSizeBytes,
                follow: options.follow,
              },
            );
            if (options.rebuild || !isCollectionIndexed(existing)) {
              resetWorkspaceIndexStorage(location);
            }

            const existingAfterRebuild = options.rebuild ? null : existing;
            if (isCollectionIndexed(existingAfterRebuild)) {
              assertCollectionEmbeddingMatchesCurrentModel(
                existingAfterRebuild,
                embeddingModel,
                "zg index --rebuild",
              );
            }

            const manifest = prepareWorkspaceManifest(
              location,
              existingAfterRebuild,
              rootPaths,
              embeddingModel,
              existingRuntime,
            );
            const collection = new Collection(
              manifest,
              embeddingModel,
              false,
              location.filesPath,
            );
            writeWorkspaceManifest(location.home, manifest);

            try {
              const releaseWriterContext = options.onWriterContext?.(
                (contextOptions) =>
                  this.contextFromWriterCollection(
                    root,
                    collection,
                    contextOptions,
                  ),
              );
              try {
                const result = await collection.index({
                  rebuild: false,
                  embeddingConcurrency: options.embeddingConcurrency,
                  onProgress: options.onProgress,
                  changedPaths: options.changedPaths,
                  signal: options.signal,
                });
                writeWorkspaceManifest(location.home, {
                  ...manifest,
                  embeddingRuntime: embeddingRuntimeAfterSuccessfulIndex(
                    existing,
                    existingRuntime,
                    embeddingModel,
                    effectiveRuntime,
                    this.options,
                  ),
                  updatedTime: Date.now(),
                });
                return result;
              } catch (error) {
                writeWorkspaceManifest(location.home, {
                  ...manifest,
                  embeddingRuntime: existingRuntime,
                  updatedTime: Date.now(),
                });
                throw error;
              } finally {
                await releaseWriterContext?.();
              }
            } finally {
              collection.close();
            }
          },
        ),
      );
    } finally {
      daemonWritePermit?.release();
    }
  }

  async dropIndex(options: ZvecGrepInfoOptions = {}): Promise<boolean> {
    this.ensureOpen();
    const root = resolveZvecGrepRoot(options.root ?? this.root);
    const daemonWritePermit = assertDaemonWriteAllowed(
      root,
      this.options.daemonInstanceToken,
    );
    const location = workspaceIndexLocation(root);
    try {
      if (!readWorkspaceManifest(location.home)) {
        return false;
      }

      return await withHomeWriteLock(location.home, "index.drop", async () => {
        resetWorkspaceIndexStorage(location);
        return true;
      });
    } finally {
      daemonWritePermit?.release();
    }
  }

  async disableIndex(
    options: ZvecGrepInfoOptions = {},
  ): Promise<ZvecGrepInfoResult> {
    this.ensureOpen();
    const root = resolveZvecGrepRoot(options.root ?? this.root);
    const daemonWritePermit = assertDaemonWriteAllowed(
      root,
      this.options.daemonInstanceToken,
    );
    const location = workspaceIndexLocation(root);
    try {
      await withHomeWriteLock(location.home, "index.disable", async () => {
        const existing = readWorkspaceManifest(location.home);
        const now = Date.now();
        writeWorkspaceManifest(location.home, {
          manifestVersion: CURRENT_MANIFEST_VERSION,
          id: existing?.id ?? randomUUID(),
          name: existing?.name ?? workspaceDisplayName(root),
          path: location.home,
          rootPaths: existing?.rootPaths ?? validateRootPaths([root]),
          indexPolicy: "disabled",
          embedding: null,
          indexVersion: null,
          createdTime: existing?.createdTime ?? now,
          updatedTime: now,
          embeddingRuntime: existing?.embeddingRuntime ?? {},
        });
      });

      return this.info({ root });
    } finally {
      daemonWritePermit?.release();
    }
  }

  async context(
    options: ZvecGrepContextOptions,
  ): Promise<ZvecGrepContextResult> {
    this.ensureOpen();
    return await this.withEmbeddingModelOperation(async () => {
      const timings = new TimingCollector();
      const result = await timings.time("total", () =>
        this.contextWithTimings(options, timings),
      );
      return withContextTimings(result, timings);
    });
  }

  private async contextWithTimings(
    options: ZvecGrepContextOptions,
    timings: TimingCollector,
  ): Promise<ZvecGrepContextResult> {
    const request = normalizeContextRequest(options);

    const startRoot = resolveZvecGrepRoot(options.root ?? this.root);
    if (options.rg) {
      return this.contextFromRg(startRoot, request, options, timings);
    }

    assertNearestWorkspaceHomeUnlocked(startRoot, "context");
    const nearest = findNearestWorkspaceCollection(startRoot);
    if (nearest) {
      const { location, info } = nearest;
      if (info.indexPolicy === "disabled") {
        throw workspaceIndexDisabledError(location.root);
      }

      if (!isCollectionIndexed(info) || !hasWorkspaceIndex(location)) {
        throw workspaceIndexMissingError(
          location.root,
          info.indexPolicy ?? "enabled",
        );
      }

      if (options.autoUpdate !== false) {
        await this.refreshWorkspaceIndexForContext(location, options, timings);
      }

      return await withHomeReadLock(location.home, "context", () =>
        this.contextFromWorkspaceIndex(location, request, options, timings),
      );
    }

    throw workspaceIndexMissingError(startRoot, "undecided");
  }

  async info(options: ZvecGrepInfoOptions = {}): Promise<ZvecGrepInfoResult> {
    this.ensureOpen();
    const startRoot = resolveZvecGrepRoot(options.root ?? this.root);
    assertNearestWorkspaceHomeUnlocked(startRoot, "info");
    const nearest = findNearestWorkspaceCollection(startRoot);

    if (!nearest) {
      const location = workspaceIndexLocation(startRoot);

      return {
        root: startRoot,
        indexed: false,
        indexPolicy: "undecided",
        home: location.home,
        indexPath: location.indexPath,
        source: "unindexed",
        suggestion: "zg index or zg query --rg",
      };
    }

    return await withHomeReadLock(nearest.location.home, "info", async () => {
      const collection = readWorkspaceManifest(nearest.location.home);
      const indexed =
        collection !== null &&
        collection.indexPolicy !== "disabled" &&
        isCollectionIndexed(collection) &&
        hasWorkspaceIndex(nearest.location);

      return {
        root: nearest.location.root,
        indexed,
        indexPolicy: collection?.indexPolicy ?? "undecided",
        home: nearest.location.home,
        indexPath: nearest.location.indexPath,
        source: indexed ? "index" : "unindexed",
        collection: collection
          ? collectionInfoFromWorkspaceManifest(collection)
          : undefined,
        status:
          indexed && options.includeStatus !== false
            ? await collectionStatus(collection, nearest.location)
            : null,
        suggestion: workspaceInfoSuggestion(collection),
      };
    });
  }

  async close(): Promise<void> {
    const models = new Set<EmbeddingModel>([
      ...(this.embeddingModel &&
      this.options.embeddingModelOwnership !== "borrowed"
        ? [this.embeddingModel]
        : []),
      ...this.recoveredEmbeddingModels.values(),
      ...this.retiredEmbeddingModels,
    ]);
    this.recoveredEmbeddingModels.clear();
    this.retiredEmbeddingModels.clear();

    for (const model of models) {
      await model.dispose();
    }

    this.closed = true;
  }

  private async contextFromWorkspaceIndex(
    location: WorkspaceIndexLocation,
    request: NormalizedContextRequest,
    options: ZvecGrepContextOptions,
    timings: TimingCollector,
  ): Promise<ZvecGrepContextResult> {
    const info = readWorkspaceManifest(location.home);
    if (!info) {
      throw new EngineError("Workspace index manifest not found", {
        code: "ZVEC_GREP.ENGINE.MANIFEST.NOT_FOUND",
      });
    }

    const collection = this.openCollectionForSearch(
      info,
      request,
      location,
      info.embeddingRuntime,
    );
    try {
      return await this.contextFromCollection({
        root: location.root,
        request,
        collection,
        options,
        timings,
      });
    } finally {
      collection.close();
    }
  }

  private async refreshWorkspaceIndexForContext(
    location: WorkspaceIndexLocation,
    options: ZvecGrepContextOptions,
    timings: TimingCollector,
  ): Promise<void> {
    const daemonWritePermit = assertDaemonWriteAllowed(
      location.root,
      this.options.daemonInstanceToken,
    );
    try {
      const needsRefresh = await timings.time("status_scan", () =>
        withHomeReadLock(location.home, "context.status", async () => {
          const manifest = readWorkspaceManifest(location.home);
          const status = manifest
            ? await collectionStatus(manifest, location)
            : null;
          return indexStatusNeedsRefresh(status);
        }),
      );

      if (!needsRefresh) {
        return;
      }

      await timings.time("auto_update", () =>
        withHomeWriteLock(location.home, "context.refresh", async () => {
          const existing = readWorkspaceManifest(location.home);
          if (!existing) {
            return;
          }

          const stillNeedsRefresh = await timings.time(
            "refresh_status_scan",
            () => collectionNeedsRefresh(location),
          );
          if (!stillNeedsRefresh) {
            return;
          }

          const workspaceRuntime = existing.embeddingRuntime;
          const embeddingModel = this.embeddingModelForIndex(
            existing,
            "context.refresh",
            workspaceRuntime,
          );
          assertCollectionEndpointMatchesCurrentRuntime(
            existing,
            workspaceRuntime,
            effectiveEmbeddingRuntime(
              this.options,
              embeddingModel,
              workspaceRuntime,
            ),
            "zg index --endpoint <url> --rebuild",
          );
          assertCollectionEmbeddingMatchesCurrentModel(
            existing,
            embeddingModel,
            "zg index --rebuild",
          );

          const collection = new Collection(
            existing,
            embeddingModel,
            false,
            location.filesPath,
          );
          try {
            const result = await collection.index({
              embeddingConcurrency: options.embeddingConcurrency,
              onProgress: options.onAutoUpdateProgress,
            });
            timings.addEntries(result.timings, "auto_update_");
          } finally {
            collection.close();
          }
        }),
      );
    } finally {
      daemonWritePermit?.release();
    }
  }

  private async contextFromCollection(input: {
    root: string;
    request: NormalizedContextRequest;
    collection: Collection;
    options: ZvecGrepContextOptions;
    timings: TimingCollector;
  }): Promise<ZvecGrepContextResult> {
    return contextFromOpenCollection(input);
  }

  private async contextFromWriterCollection(
    root: string,
    collection: Collection,
    options: ZvecGrepContextOptions,
  ): Promise<ZvecGrepContextResult> {
    return await this.withEmbeddingModelOperation(async () => {
      const timings = new TimingCollector();
      const request = normalizeContextRequest(options);
      const result = await timings.time("total", () =>
        contextFromOpenCollection({
          root,
          request,
          collection,
          options: { ...options, autoUpdate: false },
          timings,
        }),
      );
      return withContextTimings(result, timings);
    });
  }

  private async contextFromRg(
    root: string,
    request: NormalizedContextRequest,
    options: ZvecGrepContextOptions,
    timings: TimingCollector,
  ): Promise<ZvecGrepContextResult> {
    let rgResult;
    try {
      rgResult = await timings.time("rg_search", () =>
        runRgSearch({
          root,
          patterns: request.rgPatterns,
          paths: options.rgPaths,
          limit: options.limit,
          includePaths: options.includePaths,
          excludePaths: options.excludePaths,
          globs: options.globs,
          insensitiveGlobs: options.insensitiveGlobs,
          fileTypes: options.fileTypes,
          excludedFileTypes: options.excludedFileTypes,
          hidden: options.hidden,
          noIgnore: options.noIgnore,
          ignoreFiles: options.ignoreFiles,
          maxDepth: options.maxDepth,
          maxFileSizeBytes: options.maxFileSizeBytes,
          follow: options.follow,
          modifiedAfter: options.modifiedAfter,
          modifiedBefore: options.modifiedBefore,
          rgOptions: options.rgOptions,
        }),
      );
    } catch (cause) {
      throw new EngineError("Search failed", {
        code: "ZVEC_GREP.ENGINE.SEARCH.FAILED",
        context: errorDetails([detail("source", "rg"), detail("root", root)]),
        cause,
      });
    }

    const structuralEnrichment = await timings.time(
      "structure_enrichment",
      () => enrichLexicalItemsWithStructure(root, rgResult.items),
    );
    const items = dedupeAndRerankContextItems(structuralEnrichment.items);
    const emptyReason =
      items.length === 0 ? rgEmptyReason(rgResult.diagnostics) : undefined;

    return {
      query: request.displayQuery,
      root,
      source: "rg",
      coverage: rgResult.diagnostics.truncated
        ? "rg_truncated"
        : "rg_exhaustive",
      items,
      diagnostics: {
        emptyReason,
        rg: rgResult.diagnostics,
        structure: structuralEnrichment.diagnostics,
      },
    };
  }

  private openCollectionForSearch(
    info: CollectionInfo,
    request: NormalizedContextRequest,
    location: WorkspaceIndexLocation,
    workspaceRuntime: EmbeddingRuntimeConfig,
  ): Collection {
    return new Collection(
      info,
      this.embeddingModelForSearch(
        indexedEmbeddingSchema(info),
        request,
        workspaceRuntime,
        info,
      ),
      true,
      location.filesPath,
    );
  }

  private embeddingModelForSearch(
    schema: CollectionEmbeddingSchema,
    request: NormalizedContextRequest,
    workspaceRuntime: EmbeddingRuntimeConfig,
    info: CollectionInfo,
  ): EmbeddingModel | undefined {
    if (!request.routes.some((route) => route.mode === "vector")) {
      return undefined;
    }

    const identity = {
      provider: schema.provider,
      name: schema.model,
    };
    const effectiveRuntime = resolveEmbeddingRuntimeOptions(
      embeddingModelReference(identity),
      this.options,
      workspaceRuntime,
      readGlobalConfig(),
    );
    assertSearchEndpointMatchesWorkspace(
      info,
      workspaceRuntime,
      effectiveRuntime,
    );

    if (this.embeddingModel) {
      return this.embeddingModel;
    }

    return this.recoverEmbeddingModel(schema, workspaceRuntime);
  }

  private embeddingModelForIndex(
    existing: CollectionInfo | null,
    operation: string,
    workspaceRuntime: EmbeddingRuntimeConfig = {},
  ): EmbeddingModel {
    if (
      isCollectionIndexed(existing) &&
      !this.embeddingModel &&
      !this.options.embedding
    ) {
      return this.recoverEmbeddingModel(existing.embedding, workspaceRuntime);
    }

    const config = readGlobalConfig();
    const reference =
      this.options.embedding ??
      config.defaults?.embedding ??
      nonEmptyEnvironmentValue(process.env.ZVEC_GREP_EMBEDDING) ??
      (this.options.defaultEmbedding === true
        ? DEFAULT_LOCAL_EMBEDDING
        : undefined);
    const referenceIdentity = reference
      ? parseEmbeddingModelReference(reference)
      : undefined;
    const selectedWorkspaceRuntime =
      isCollectionIndexed(existing) &&
      referenceIdentity &&
      existing.embedding.provider !== referenceIdentity.provider
        ? {}
        : workspaceRuntime;
    return (
      this.embeddingModel ??
      (reference
        ? this.embeddingModelFromReference(
            reference,
            config,
            selectedWorkspaceRuntime,
          )
        : undefined) ??
      this.requireEmbeddingModel(operation)
    );
  }

  private recoverEmbeddingModel(
    schema: CollectionEmbeddingSchema,
    workspaceRuntime: EmbeddingRuntimeConfig = {},
  ): EmbeddingModel {
    const config = readGlobalConfig();
    const identity = {
      provider: schema.provider,
      name: schema.model,
    };
    const reference = embeddingModelReference(identity);
    const options = providerOptions(
      this.options,
      identity,
      config,
      workspaceRuntime,
    );
    const key = `${reference}/${providerOptionsFingerprint(options)}`;
    return this.cachedEmbeddingModel(key, () =>
      createServiceEmbeddingModel(reference, options, this.options),
    );
  }

  private embeddingModelFromReference(
    reference: string,
    config: ZvecGrepGlobalConfig = readGlobalConfig(),
    workspaceRuntime: EmbeddingRuntimeConfig = {},
  ): EmbeddingModel {
    const identity = parseEmbeddingModelReference(reference);
    const options = providerOptions(
      this.options,
      identity,
      config,
      workspaceRuntime,
    );
    const key = `configured/${reference}/${providerOptionsFingerprint(options)}`;
    return this.cachedEmbeddingModel(key, () =>
      createServiceEmbeddingModel(reference, options, this.options),
    );
  }

  private cachedEmbeddingModel(
    key: string,
    create: () => EmbeddingModel,
  ): EmbeddingModel {
    const cached = this.recoveredEmbeddingModels.get(key);
    if (cached) {
      this.recoveredEmbeddingModels.delete(key);
      this.recoveredEmbeddingModels.set(key, cached);
      return cached;
    }

    const model = create();
    this.recoveredEmbeddingModels.set(key, model);
    this.trimRecoveredEmbeddingModels();
    return model;
  }

  private trimRecoveredEmbeddingModels(): void {
    while (
      this.recoveredEmbeddingModels.size > MAX_RECOVERED_EMBEDDING_MODELS
    ) {
      const oldestKey = this.recoveredEmbeddingModels.keys().next().value;
      if (oldestKey === undefined) {
        return;
      }

      const model = this.recoveredEmbeddingModels.get(oldestKey);
      this.recoveredEmbeddingModels.delete(oldestKey);
      if (model) {
        this.retiredEmbeddingModels.add(model);
      }
    }
  }

  private async withEmbeddingModelOperation<T>(
    operation: () => Promise<T>,
  ): Promise<T> {
    this.activeEmbeddingModelOperations += 1;
    try {
      return await operation();
    } finally {
      this.activeEmbeddingModelOperations -= 1;
      if (this.activeEmbeddingModelOperations === 0) {
        await this.disposeRetiredEmbeddingModels();
      }
    }
  }

  private async disposeRetiredEmbeddingModels(): Promise<void> {
    const models = [...this.retiredEmbeddingModels];
    this.retiredEmbeddingModels.clear();
    for (const model of models) {
      await model.dispose();
    }
  }

  private requireEmbeddingModel(operation: string): EmbeddingModel {
    if (!this.embeddingModel) {
      throw new EngineError("zvec-grep operation requires an embedding model", {
        code: "ZVEC_GREP.ENGINE.SERVICE.EMBEDDING_MODEL_REQUIRED",
        context: errorDetails([
          detail("operation", operation),
          detail(
            "hint",
            `Pass "--embedding <model>", set ZVEC_GREP_EMBEDDING, or configure defaults.embedding in ${globalConfigPath()}. Existing indexes can run "zg index" without --embedding to reuse the stored schema.`,
          ),
          detail(
            "examples",
            "local/embeddinggemma-300m, qwen/text-embedding-v4",
          ),
        ]),
      });
    }

    return this.embeddingModel;
  }

  private ensureOpen(): void {
    if (this.closed) {
      throw new EngineError("zvec-grep service is already closed", {
        code: "ZVEC_GREP.ENGINE.SERVICE.CLOSED",
      });
    }
  }
}

async function contextFromOpenCollection(input: {
  root: string;
  request: NormalizedContextRequest;
  collection: Collection;
  options: ZvecGrepContextOptions;
  timings: TimingCollector;
}): Promise<ZvecGrepContextResult> {
  const searches: SearchPlanResult[] = [];
  const groups = input.options.fuse
    ? [{ routes: input.request.routes }]
    : input.request.groups;
  const limit = contextGroupLimit(input.options.limit, groups.length);

  for (const group of groups) {
    const search = await input.collection.searchPlan({
      routes: group.routes,
      limit,
      trace: input.options.trace,
      preferSymbol: input.options.preferSymbol,
      symbolTypes: input.options.symbolTypes,
      includePaths: input.options.includePaths,
      excludePaths: input.options.excludePaths,
      globs: input.options.globs,
      insensitiveGlobs: input.options.insensitiveGlobs,
      fileTypes: input.options.fileTypes,
      excludedFileTypes: input.options.excludedFileTypes,
      modifiedAfter: input.options.modifiedAfter,
      modifiedBefore: input.options.modifiedBefore,
    });
    input.timings.addEntries(search.timings);
    searches.push(search);
  }

  const items = dedupeAndRerankContextItems(
    searches.flatMap((search) => searchPlanToContextItems(search, input.root)),
  );

  return {
    query: input.request.displayQuery,
    root: input.root,
    source: "index",
    coverage: "ranked_sample",
    collection: {
      id: input.collection.info.id,
      name: input.collection.info.name,
      path: input.collection.info.path,
    },
    items,
    diagnostics: {
      emptyReason: items.length === 0 ? "no_matches" : undefined,
      index: {
        hitsReturned: items.length,
        routes: searches.flatMap((search) => search.plan.routes),
      },
    },
  };
}

async function withHomeReadLock<T>(
  home: string,
  operation: string,
  task: () => Promise<T>,
): Promise<T> {
  const lock = acquireHomeLock(home, "read", operation);
  try {
    return await task();
  } finally {
    lock.release();
  }
}

async function withHomeWriteLock<T>(
  home: string,
  operation: string,
  task: () => Promise<T>,
): Promise<T> {
  const lock = acquireHomeLock(home, "write", operation);
  try {
    return await task();
  } finally {
    lock.release();
  }
}

async function collectionNeedsRefresh(
  location: WorkspaceIndexLocation,
): Promise<boolean> {
  const manifest = readWorkspaceManifest(location.home);
  const status = manifest ? await collectionStatus(manifest, location) : null;
  return indexStatusNeedsRefresh(status);
}

function acquireHomeLock(
  home: string,
  mode: "read" | "write",
  operation: string,
): FileLock {
  return acquireReadWriteLock(homeLockPath(home), mode, { operation });
}

function assertHomeUnlocked(home: string, operation: string): void {
  assertNoWriteLock(homeLockPath(home), operation);
}

function assertNearestWorkspaceHomeUnlocked(
  start: string,
  operation: string,
): void {
  let current = resolve(start);

  while (true) {
    assertHomeUnlocked(workspaceHome(current), operation);

    const parent = dirname(current);
    if (parent === current) {
      return;
    }

    current = parent;
  }
}

function homeLockPath(home: string): string {
  return join(home, "locks", "home");
}

function rgEmptyReason(
  diagnostics: Awaited<ReturnType<typeof runRgSearch>>["diagnostics"],
): NonNullable<ZvecGrepContextResult["diagnostics"]["emptyReason"]> {
  return diagnostics.missingPaths &&
    diagnostics.missingPaths.length > 0 &&
    diagnostics.searchedPaths &&
    diagnostics.searchedPaths.length === 0
    ? "no_searchable_files"
    : "no_matches";
}

function withContextTimings(
  result: ZvecGrepContextResult,
  timings: TimingCollector,
): ZvecGrepContextResult {
  const entries = timings.entries();
  if (entries.length === 0) {
    return result;
  }

  return {
    ...result,
    diagnostics: {
      ...result.diagnostics,
      timings: entries,
    },
  };
}

type WorkspaceCollectionRecord = {
  location: WorkspaceIndexLocation;
  info: WorkspaceManifest;
};

function findNearestWorkspaceCollection(
  start: string,
): WorkspaceCollectionRecord | null {
  const location = findNearestWorkspace(start);
  if (!location) return null;
  const info = readWorkspaceManifest(location.home);
  return info ? { location, info } : null;
}

function workspaceInfoSuggestion(
  collection: CollectionInfo | null,
): string | undefined {
  if (!collection) {
    return "zg index or zg query --rg";
  }

  if (collection.indexPolicy === "disabled") {
    return "zg query --rg";
  }

  if (!isCollectionIndexed(collection)) {
    return "zg index";
  }

  return undefined;
}

function workspaceIndexMissingError(
  root: string,
  policy: "undecided" | "enabled",
): EngineError {
  return new EngineError("No zvec-grep index found for this workspace", {
    code: "ZVEC_GREP.ENGINE.SERVICE.WORKSPACE_INDEX_NOT_FOUND",
    context: errorDetails([
      detail("root", root),
      detail("policy", policy),
      detail(
        "hint",
        policy === "undecided"
          ? "Ask the user whether to build an index with zg index, or use zg query --rg for no-index search."
          : "Run zg index to build the enabled workspace index, or use zg query --rg for no-index search.",
      ),
      detail(
        "agent_prompt",
        policy === "undecided"
          ? "Ask the user whether this workspace should be indexed. If yes, run zg index --embedding <model> with appropriate -g/--glob and -t/--type filters; otherwise use zg query --rg for immediate no-index search."
          : "This workspace is marked index-enabled but has no built index. Ask before running zg index if an embedding model or cost is involved; otherwise use zg query --rg for immediate no-index search.",
      ),
    ]),
  });
}

function workspaceIndexDisabledError(root: string): EngineError {
  return new EngineError("The zvec-grep index is disabled for this workspace", {
    code: "ZVEC_GREP.ENGINE.SERVICE.WORKSPACE_INDEX_DISABLED",
    context: errorDetails([
      detail("root", root),
      detail("policy", "disabled"),
      detail(
        "hint",
        "Use zg query --rg for no-index search. Run zg index only if the user explicitly decides to index this workspace.",
      ),
      detail("agent_action", "do_not_build_index"),
    ]),
  });
}

function resolveIndexRootPaths(
  existing: CollectionInfo | null,
  requested: readonly (string | RootPath)[] | undefined,
  fallbackRoot: string,
  options: {
    resetPaths: boolean;
    includePaths?: readonly string[];
    excludePaths?: readonly string[];
    globs?: readonly string[];
    insensitiveGlobs?: readonly string[];
    fileTypes?: readonly string[];
    excludedFileTypes?: readonly string[];
    hidden?: boolean;
    noIgnore?: boolean;
    ignoreFiles?: readonly string[];
    maxDepth?: number;
    maxFileSizeBytes?: number;
    follow?: boolean;
  },
): readonly (string | RootPath)[] {
  let rootPaths = requested
    ? inheritRequestedRootPathSettings(requested, existing?.rootPaths)
    : (existing?.rootPaths ?? [fallbackRoot]);

  if (options.resetPaths) {
    rootPaths = rootPaths.map(resetRootPathFilters);
  }

  if (
    options.includePaths !== undefined ||
    options.excludePaths !== undefined ||
    options.globs !== undefined ||
    options.insensitiveGlobs !== undefined ||
    options.fileTypes !== undefined ||
    options.excludedFileTypes !== undefined ||
    options.hidden !== undefined ||
    options.noIgnore !== undefined ||
    options.ignoreFiles !== undefined ||
    options.maxDepth !== undefined ||
    options.maxFileSizeBytes !== undefined ||
    options.follow !== undefined
  ) {
    rootPaths = rootPaths.map((rootPath) =>
      applyRootPathOverrides(rootPath, options),
    );
  }

  return rootPaths;
}

function inheritRequestedRootPathSettings(
  requested: readonly (string | RootPath)[],
  existing: readonly RootPath[] | undefined,
): readonly (string | RootPath)[] {
  if (!existing?.length) {
    return requested;
  }

  return requested.map((rootPath) => {
    const absolutePath = normalizePath(
      typeof rootPath === "string" ? rootPath : rootPath.absolutePath,
    );
    const inherited = existing.find(
      (candidate) => normalizePath(candidate.absolutePath) === absolutePath,
    );
    if (!inherited) {
      return rootPath;
    }

    if (typeof rootPath === "string") {
      return { ...inherited, absolutePath };
    }

    return {
      ...inherited,
      ...rootPath,
      absolutePath,
      include: rootPath.include ?? inherited.include,
      exclude: rootPath.exclude ?? inherited.exclude,
      globs: rootPath.globs ?? inherited.globs,
      insensitiveGlobs: rootPath.insensitiveGlobs ?? inherited.insensitiveGlobs,
      fileTypes: rootPath.fileTypes ?? inherited.fileTypes,
      excludedFileTypes:
        rootPath.excludedFileTypes ?? inherited.excludedFileTypes,
      hidden: rootPath.hidden ?? inherited.hidden,
      noIgnore: rootPath.noIgnore ?? inherited.noIgnore,
      ignoreFiles: rootPath.ignoreFiles ?? inherited.ignoreFiles,
      maxDepth: rootPath.maxDepth ?? inherited.maxDepth,
      maxFileSizeBytes: rootPath.maxFileSizeBytes ?? inherited.maxFileSizeBytes,
      follow: rootPath.follow ?? inherited.follow,
    };
  });
}

function resetRootPathFilters(rootPath: string | RootPath): string | RootPath {
  if (typeof rootPath === "string") {
    return rootPath;
  }

  return {
    absolutePath: rootPath.absolutePath,
    recursive: rootPath.recursive,
  };
}

function applyRootPathOverrides(
  rootPath: string | RootPath,
  options: {
    includePaths?: readonly string[];
    excludePaths?: readonly string[];
    globs?: readonly string[];
    insensitiveGlobs?: readonly string[];
    fileTypes?: readonly string[];
    excludedFileTypes?: readonly string[];
    hidden?: boolean;
    noIgnore?: boolean;
    ignoreFiles?: readonly string[];
    maxDepth?: number;
    maxFileSizeBytes?: number;
    follow?: boolean;
  },
): RootPath {
  const normalized =
    typeof rootPath === "string"
      ? {
          absolutePath: rootPath,
          recursive: true,
        }
      : rootPath;

  return {
    ...normalized,
    include: options.includePaths ?? normalized.include,
    exclude: options.excludePaths ?? normalized.exclude,
    globs: options.globs ?? normalized.globs,
    insensitiveGlobs: options.insensitiveGlobs ?? normalized.insensitiveGlobs,
    fileTypes: options.fileTypes ?? normalized.fileTypes,
    excludedFileTypes:
      options.excludedFileTypes ?? normalized.excludedFileTypes,
    hidden: options.hidden ?? normalized.hidden,
    noIgnore: options.noIgnore ?? normalized.noIgnore,
    ignoreFiles: options.ignoreFiles ?? normalized.ignoreFiles,
    maxDepth: options.maxDepth ?? normalized.maxDepth,
    maxFileSizeBytes: options.maxFileSizeBytes ?? normalized.maxFileSizeBytes,
    follow: options.follow ?? normalized.follow,
  };
}

function prepareWorkspaceManifest(
  location: WorkspaceIndexLocation,
  existing: WorkspaceManifest | null,
  rootPaths: readonly (string | RootPath)[],
  embeddingModel: EmbeddingModel,
  embeddingRuntime: EmbeddingRuntimeConfig,
): WorkspaceManifest {
  const now = Date.now();
  return {
    manifestVersion: CURRENT_MANIFEST_VERSION,
    id: existing?.id ?? randomUUID(),
    name: existing?.name ?? workspaceDisplayName(location.root),
    path: location.home,
    rootPaths: validateRootPaths(rootPaths),
    indexPolicy: "enabled",
    embedding: currentEmbeddingSchema(embeddingModel),
    indexVersion: CURRENT_INDEX_VERSION,
    createdTime: existing?.createdTime ?? now,
    updatedTime: now,
    embeddingRuntime,
  };
}

function currentEmbeddingSchema(
  embeddingModel: EmbeddingModel,
): CollectionEmbeddingSchema {
  return {
    provider: embeddingModel.info.provider,
    model: embeddingModel.info.name,
    dimension: embeddingModel.info.dimension,
    metric: embeddingModel.info.metric,
  };
}

function workspaceDisplayName(root: string): string {
  return basename(normalizePath(root)) || "workspace";
}

async function collectionStatus(
  info: CollectionInfo,
  location: WorkspaceIndexLocation,
) {
  if (
    info.indexPolicy === "disabled" ||
    !isCollectionIndexed(info) ||
    !hasWorkspaceIndex(location)
  ) {
    return null;
  }

  const collection = new Collection(info, undefined, true, location.filesPath);
  try {
    return await collection.status();
  } finally {
    collection.close();
  }
}

function indexedEmbeddingSchema(
  info: CollectionInfo,
): CollectionEmbeddingSchema {
  if (isCollectionIndexed(info)) {
    return info.embedding;
  }

  throw new EngineError("zvec-grep index has not been built", {
    code: "ZVEC_GREP.ENGINE.SERVICE.INDEX_MISSING",
    context: errorDetails([
      collectionDetail(info.name),
      detail("hint", "Run zg index to build this index."),
    ]),
  });
}

function providerOptions(
  options: CreateZvecGrepOptions,
  identity: EmbeddingModelIdentity,
  config: ZvecGrepGlobalConfig = readGlobalConfig(),
  workspaceRuntime: EmbeddingRuntimeConfig = {},
): CreateEmbeddingModelOptions & { apiKey: string } {
  const reference = embeddingModelReference(identity);
  const runtime = resolveEmbeddingRuntimeOptions(
    reference,
    options,
    workspaceRuntime,
    config,
  );
  return {
    apiKey: runtime.apiKey,
    endpoint: runtime.endpoint,
    modelCacheDir:
      options.modelCacheDir ??
      config.defaults?.modelCacheDir ??
      process.env.ZVEC_GREP_MODEL_CACHE ??
      join(options.home ?? defaultHome(), "models"),
    device: runtime.device,
  };
}

function createServiceEmbeddingModel(
  reference: string,
  modelOptions: CreateEmbeddingModelOptions,
  serviceOptions: CreateZvecGrepOptions,
): EmbeddingModel {
  const model = createEmbeddingModel(reference, modelOptions);
  if (model.info.provider !== "qwen") {
    return model;
  }

  const endpoint = model.info.endpoint;
  if (endpoint === undefined) {
    throw new EngineError(
      "Remote embedding model did not provide an endpoint",
      {
        code: "ZVEC_GREP.ENGINE.MODELS.REMOTE_ENDPOINT_NOT_IMPLEMENTED",
        context: `model=${model.info.reference}`,
      },
    );
  }
  const authorize = remoteEmbeddingAuthorizationGuard({
    store: new RemoteEmbeddingAuthorizationStore({
      signingKeyPath: serviceOptions.authorizationSigningKeyPath,
    }),
  });

  const authorizedModel: EmbeddingModel = {
    info: model.info,
    async embed(contents, embedOptions) {
      await authorize({
        provider: model.info.provider,
        model: model.info.name,
        endpoint,
        purpose: embedOptions?.purpose ?? EmbeddingPurpose.Document,
        contentKinds: contents.map((content) => content.kind),
        contentCount: contents.length,
      });
      return await model.embed(contents, embedOptions);
    },
    dispose: () => model.dispose(),
  };
  return authorizedModel;
}

function parseEmbeddingModelReference(
  reference: string,
): EmbeddingModelIdentity {
  const separator = reference.indexOf("/");
  if (separator <= 0 || separator === reference.length - 1) {
    throw new EngineError("Embedding model reference is invalid", {
      code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_INVALID_REFERENCE",
      context: `reference=${reference}`,
    });
  }
  return {
    provider: reference.slice(0, separator),
    name: reference.slice(separator + 1),
  };
}

function embeddingModelReference(identity: EmbeddingModelIdentity): string {
  return `${identity.provider}/${identity.name}`;
}

function providerOptionsFingerprint(options: {
  apiKey: string;
  endpoint?: string;
  modelCacheDir?: string;
  device?: "auto" | "cpu" | "metal" | "vulkan" | "cuda";
}): string {
  const publicOptionsFingerprint = createHash("sha256")
    .update(
      JSON.stringify([options.endpoint, options.modelCacheDir, options.device]),
    )
    .digest("hex");
  return `${providerApiKeyIdentity(options.apiKey)}:${publicOptionsFingerprint}`;
}

function providerApiKeyIdentity(apiKey: string): string {
  return createHmac("sha256", PROVIDER_API_KEY_IDENTITY_SECRET)
    .update(apiKey)
    .digest("hex");
}

function effectiveEmbeddingRuntime(
  options: CreateZvecGrepOptions,
  model: EmbeddingModel,
  workspaceRuntime: EmbeddingRuntimeConfig,
): ResolvedEmbeddingRuntimeConfig {
  const resolved = resolveEmbeddingRuntimeOptions(
    model.info.reference,
    options,
    workspaceRuntime,
    readGlobalConfig(),
  );
  return {
    apiKey: resolved.apiKey,
    ...((model.info.endpoint ?? resolved.endpoint)
      ? { endpoint: model.info.endpoint ?? resolved.endpoint }
      : {}),
    ...(model.info.provider === "local"
      ? { device: resolved.device ?? "auto" }
      : {}),
  };
}

function runtimeForModelProvider(
  existing: CollectionInfo | null,
  model: EmbeddingModel,
  workspaceRuntime: EmbeddingRuntimeConfig,
): EmbeddingRuntimeConfig {
  return isCollectionIndexed(existing) &&
    existing.embedding.provider !== model.info.provider
    ? {}
    : workspaceRuntime;
}

function embeddingRuntimeAfterSuccessfulIndex(
  existing: CollectionInfo | null,
  existingRuntime: EmbeddingRuntimeConfig,
  model: EmbeddingModel,
  effectiveRuntime: ResolvedEmbeddingRuntimeConfig,
  explicit: CreateZvecGrepOptions,
): EmbeddingRuntimeConfig {
  const sameProvider =
    isCollectionIndexed(existing) &&
    existing.embedding.provider === model.info.provider;
  const apiKey =
    model.info.provider === "local"
      ? undefined
      : (explicit.apiKey ??
        (sameProvider ? existingRuntime.apiKey : undefined));
  return {
    ...(apiKey !== undefined ? { apiKey } : {}),
    ...(model.info.provider !== "local" &&
    effectiveRuntime.endpoint !== undefined
      ? { endpoint: effectiveRuntime.endpoint }
      : {}),
    ...(model.info.provider === "local"
      ? { device: effectiveRuntime.device ?? "auto" }
      : {}),
  };
}

function assertCollectionEndpointMatchesCurrentRuntime(
  info: CollectionInfo | null,
  workspaceRuntime: EmbeddingRuntimeConfig,
  effectiveRuntime: ResolvedEmbeddingRuntimeConfig,
  rebuildCommand: string,
): void {
  if (
    !isCollectionIndexed(info) ||
    workspaceRuntime.endpoint === effectiveRuntime.endpoint
  ) {
    return;
  }
  throw new EngineError(
    "Existing zvec-grep index uses a different embedding endpoint",
    {
      code: "ZVEC_GREP.ENGINE.SERVICE.EMBEDDING_ENDPOINT_CHANGE_REQUIRES_REBUILD",
      context: errorDetails([
        collectionDetail(info.name),
        detail(
          "hint",
          `Run "${rebuildCommand}" to rebuild this index with the requested endpoint.`,
        ),
      ]),
    },
  );
}

function assertSearchEndpointMatchesWorkspace(
  info: CollectionInfo,
  workspaceRuntime: EmbeddingRuntimeConfig,
  effectiveRuntime: ResolvedEmbeddingRuntimeConfig,
): void {
  if (workspaceRuntime.endpoint === effectiveRuntime.endpoint) {
    return;
  }
  throw new EngineError(
    "Search cannot override the embedding endpoint of an existing index",
    {
      code: "ZVEC_GREP.ENGINE.SERVICE.SEARCH_ENDPOINT_CHANGE_REQUIRES_REBUILD",
      context: errorDetails([
        collectionDetail(info.name),
        detail(
          "hint",
          'Run "zg index --endpoint <url> --rebuild" before searching with this endpoint.',
        ),
      ]),
    },
  );
}

function nonEmptyEnvironmentValue(
  value: string | undefined,
): string | undefined {
  const normalized = value?.trim();
  return normalized ? normalized : undefined;
}

function assertCollectionEmbeddingMatchesCurrentModel(
  info: CollectionInfo | null,
  model: EmbeddingModel,
  rebuildCommand: string,
): void {
  if (!isCollectionIndexed(info)) {
    return;
  }

  const expected = info.embedding;
  const changed =
    expected.provider !== model.info.provider ||
    expected.model !== model.info.name ||
    expected.dimension !== model.info.dimension ||
    expected.metric !== model.info.metric;

  if (!changed) {
    return;
  }

  throw new EngineError(
    "Existing zvec-grep index uses a different embedding model",
    {
      code: "ZVEC_GREP.ENGINE.SERVICE.EMBEDDING_SCHEMA_CHANGE_REQUIRES_REBUILD",
      context: errorDetails([
        collectionDetail(info.name),
        detail("existing", `${expected.provider}/${expected.model}`),
        detail("requested", model.info.reference),
        detail(
          "hint",
          `Run "${rebuildCommand}" to rebuild this index with the requested embedding model.`,
        ),
      ]),
    },
  );
}

function contextGroupLimit(
  limit: number | undefined,
  groupCount: number,
): number {
  if (limit !== undefined) {
    return limit;
  }

  const safeGroupCount = Math.max(1, groupCount);
  if (safeGroupCount <= 3) {
    return DEFAULT_CONTEXT_LIMIT;
  }

  return Math.max(1, Math.ceil(DEFAULT_CONTEXT_TOTAL_LIMIT / safeGroupCount));
}

type NormalizedContextRequest = {
  displayQuery: string;
  rgPatterns: string[];
  routes: SearchPlan["routes"];
  groups: NormalizedContextGroup[];
};

type NormalizedContextGroup = {
  routes: SearchPlan["routes"];
};

function normalizeContextRequest(
  options: ZvecGrepContextOptions,
): NormalizedContextRequest {
  const primaryQueries = normalizePrimaryQueries(
    options.query,
    options.queries,
  );
  const extraRoutes = normalizeContextRoutes(options.routes ?? []);

  const hasPatternFiles =
    options.rg === true && (options.rgOptions?.patternFiles?.length ?? 0) > 0;
  if (
    primaryQueries.length === 0 &&
    extraRoutes.length === 0 &&
    !hasPatternFiles
  ) {
    throw new EngineError(
      "zvec-grep context requires a non-empty query or route",
      {
        code: "ZVEC_GREP.ENGINE.SERVICE.EMPTY_QUERY",
      },
    );
  }

  const groups = contextGroups(primaryQueries, extraRoutes);
  const routes = groups.flatMap((group) => group.routes);
  const rgPatterns = [
    ...primaryQueries,
    ...extraRoutes.map((route) => route.query),
  ];
  const displayQuery =
    primaryQueries.length > 0
      ? primaryQueries.join(" | ")
      : extraRoutes.length > 0
        ? extraRoutes.map((route) => route.query).join(" | ")
        : options
            .rgOptions!.patternFiles!.map((path) => `@${path}`)
            .join(" | ");

  return {
    displayQuery,
    rgPatterns,
    routes,
    groups,
  };
}

function normalizePrimaryQueries(
  query: string | undefined,
  queries: readonly string[] | undefined,
): string[] {
  return [query, ...(queries ?? [])].flatMap((value) => {
    const normalized = normalizeOptionalQuery(value);
    return normalized ? [normalized] : [];
  });
}

function normalizeOptionalQuery(query: string | undefined): string | undefined {
  const normalized = query?.trim() ?? "";
  return normalized.length > 0 ? normalized : undefined;
}

function normalizeContextRoutes(
  routes: readonly SearchPlan["routes"][number][],
): SearchPlan["routes"] {
  return routes.map((route, index) => {
    if (route.mode !== "fts" && route.mode !== "vector") {
      throw new EngineError("zvec-grep context route has an unsupported mode", {
        code: "ZVEC_GREP.ENGINE.SERVICE.INVALID_ROUTE_MODE",
        context: `routeIndex=${index} mode=${String(route.mode)}`,
      });
    }

    const query = route.query.trim();
    if (query.length === 0) {
      throw new EngineError(
        "zvec-grep context route requires a non-empty query",
        {
          code: "ZVEC_GREP.ENGINE.SERVICE.EMPTY_ROUTE_QUERY",
          context: `routeIndex=${index} mode=${route.mode}`,
        },
      );
    }

    return {
      mode: route.mode,
      query,
    };
  });
}

function contextGroups(
  primaryQueries: readonly string[],
  extraRoutes: SearchPlan["routes"],
): NormalizedContextGroup[] {
  return [
    ...primaryQueries.map((query) => ({
      routes: [
        { mode: "fts" as const, query },
        { mode: "vector" as const, query },
      ],
    })),
    ...extraRoutes.map((route) => ({
      routes: [route],
    })),
  ];
}

function searchPlanToContextItems(
  result: SearchPlanResult,
  root: string,
): ZvecGrepContextItem[] {
  return result.hits.map((hit) => {
    const target = contextItemTarget(hit);

    return {
      kind: "indexed_entity",
      rank: hit.rank,
      file: {
        absolutePath: hit.file.absolutePath,
        relativePath:
          hit.file.relativePath || relative(root, hit.file.absolutePath) || ".",
        rootPath: hit.file.rootPath,
      },
      range: hit.entity.range,
      excerptRange: target.excerptRange,
      content: contentToText(target.content),
      contentRole: target.contentRole,
      outline: target.outline,
      status: fileFreshnessStatus(hit.file),
      score: hit.score,
      matchedBy: hit.matchedBy,
      metadata: hit.entity.metadata,
      entityId: hit.entity.id,
      trace: hit.trace,
    };
  });
}

function dedupeAndRerankContextItems(
  items: readonly ZvecGrepContextItem[],
): ZvecGrepContextItem[] {
  const seen = new Set<string>();
  const deduped: ZvecGrepContextItem[] = [];

  for (const item of items) {
    const key = contextItemDedupeKey(item);
    if (seen.has(key)) {
      continue;
    }

    seen.add(key);
    deduped.push({
      ...item,
      rank: deduped.length + 1,
    });
  }

  return deduped;
}

function contextItemDedupeKey(item: ZvecGrepContextItem): string {
  if (item.entityId) {
    return `entity:${item.entityId}`;
  }

  return ["range", item.file.absolutePath, JSON.stringify(item.range)].join(
    ":",
  );
}

type ContextItemTarget = {
  content: SearchHit["entity"]["content"];
  contentRole: "source" | "outline";
  excerptRange?: SearchHit["entity"]["range"];
  outline?: string;
};

function contextItemTarget(hit: SearchHit): ContextItemTarget {
  const evidence = hit.evidence.find((item) => !item.isEntity);
  const hasSeparateEvidence =
    evidence && !sameDisplayedContent(hit.entity, evidence);
  const content = hasSeparateEvidence ? evidence.content : hit.entity.content;
  const contentRole =
    hasSeparateEvidence || entityContentLooksLikeSource(hit.entity)
      ? "source"
      : "outline";
  const excerptRange = hasSeparateEvidence ? evidence.range : undefined;
  const outline =
    contentRole === "source" ? contextItemOutline(hit, evidence) : undefined;

  return {
    content,
    contentRole,
    excerptRange,
    outline,
  };
}

function entityContentLooksLikeSource(entity: SearchHit["entity"]): boolean {
  if (entity.content.kind !== "text" || entity.range.kind !== "text") {
    return true;
  }

  const expectedLines = entity.range.endLine - entity.range.startLine + 1;
  const actualLines = entity.content.text.split(/\r?\n/).length;

  return actualLines >= expectedLines;
}

function contextItemOutline(
  hit: SearchHit,
  evidence: SearchHit["evidence"][number] | undefined,
): string | undefined {
  if (hit.entity.content.kind !== "text") {
    return undefined;
  }

  const outline = hit.entity.content.text.trim();
  if (outline.length === 0) {
    return undefined;
  }

  if (!evidence) {
    return undefined;
  }

  if (evidence && sameDisplayedContent(hit.entity, evidence)) {
    return undefined;
  }

  if (!isUsefulOutline(hit.entity.metadata, outline)) {
    return undefined;
  }

  if (
    evidence?.content.kind === "text" &&
    evidence.content.text.trim() === outline
  ) {
    return undefined;
  }

  return outline;
}

function isUsefulOutline(
  metadata: SearchHit["entity"]["metadata"],
  outline: string,
): boolean {
  if (!metadata || metadata.kind !== "code") {
    return false;
  }

  return (
    metadata.symbolType === "class" ||
    metadata.symbolType === "interface" ||
    metadata.symbolType === "module" ||
    outline.includes("\ncalls:")
  );
}

function sameDisplayedContent(
  entity: Pick<SearchHit["entity"], "range" | "content">,
  evidence: Pick<SearchHit["entity"], "range" | "content">,
): boolean {
  return (
    JSON.stringify(entity.range) === JSON.stringify(evidence.range) &&
    contentEquals(entity.content, evidence.content)
  );
}

function contentEquals(left: Content, right: Content): boolean {
  if (left.kind !== right.kind) {
    return false;
  }

  if (left.kind === "image") {
    return (
      right.kind === "image" &&
      left.format === right.format &&
      left.data.byteLength === right.data.byteLength
    );
  }

  return right.kind === "text" && left.text === right.text;
}

function fileFreshnessStatus(file: FileInfo): "fresh" | "possibly_stale" {
  if (!file.indexStatus?.indexedTime) {
    return "possibly_stale";
  }

  try {
    const info = statSync(file.absolutePath, { throwIfNoEntry: false });
    if (!info || !info.isFile()) {
      return "possibly_stale";
    }

    if (file.indexStatus.indexedTime >= info.mtimeMs) {
      return "fresh";
    }

    if (
      file.contentHash &&
      sha256File(file.absolutePath) === file.contentHash
    ) {
      return "fresh";
    }
  } catch {
    return "possibly_stale";
  }

  return "possibly_stale";
}

function sha256File(path: string): string {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function contentToText(content: Content): string {
  if (content.kind === "text") {
    return content.text;
  }

  return `[image:${content.format} bytes=${content.data.byteLength}]`;
}
