import { createZvecGrep } from "../engine/service/index.js";
import type {
  CreateZvecGrepOptions,
  ZvecGrepExploreOptions,
  ZvecGrepExploreResult,
  ZvecGrepGraphNeighborhoodOptions,
  ZvecGrepGraphNeighborhoodResult,
  ZvecGrepInfoResult,
} from "../engine/service/types.js";
import { isEngineError } from "../engine/errors.js";
import {
  readGlobalConfig,
  resolveEmbeddingRuntimeOptions,
  type EmbeddingRuntimeConfig,
} from "../engine/config.js";
import { readWorkspaceManifest } from "../engine/manifest.js";
import { workspaceIndexLocation } from "../engine/service/root.js";
import type {
  EmbeddingModel,
  EmbeddingModelInfo,
} from "../engine/models/index.js";
import { resolveEmbeddingReference } from "../engine/models/index.js";
import type {
  FileScanDiagnostics,
  WorkspaceIndexEmbeddingSchema,
  IndexProgress,
} from "../engine/types.js";
import {
  indexCompletionForJob,
  indexCompletionFromStatus,
} from "../engine/index-status.js";
import {
  contextOptionsFromRgInput,
  normalizePlainStringList,
  type NormalizedSearchInput,
} from "../mcp/input-normalization.js";
import type {
  ZvecGrepDaemonBackend,
  ZvecGrepIndexDropResult,
  ZvecGrepIndexResult,
  ZvecGrepIndexStatusResult,
  ZvecGrepRgResult,
  ZvecGrepSearchResult,
  ZvecGrepServerStatusResult,
} from "../mcp/tools.js";
import type {
  ZvecGrepIndexInput,
  ZvecGrepIndexRequest,
  ZvecGrepIndexDropInput,
  ZvecGrepIndexStatusInput,
  ZvecGrepRgInput,
} from "../mcp/schemas.js";
import { DaemonError } from "./errors.js";
import {
  JobScheduler,
  type IndexJobSnapshot,
  type JobSchedulerOptions,
} from "./job-scheduler.js";
import {
  EmbeddingModelPool,
  type EmbeddingModelLoadRequest,
  type ModelLease,
  type EmbeddingModelPoolOptions,
} from "./model-pool.js";
import {
  IndexCoordinator,
  type IndexReconciliationProof,
} from "./index-coordinator.js";
import {
  inspectRoot,
  resolveRequestedRoot,
  RuntimeManager,
} from "./runtime-manager.js";
import type { RootRuntime } from "./root-runtime.js";
import { WatchManager, type WatchManagerOptions } from "./watch-manager.js";
import { rootIdentity, type DaemonLogger } from "./logger.js";
import {
  RemoteEmbeddingAuthorizationManager,
  RemoteEmbeddingAuthorizationStore,
  planRemoteIndexAuthorization,
  planRemoteSearchAuthorization,
  withRemoteEmbeddingOperationPermit,
  type RemoteEmbeddingAuthorizationPlan,
  type RemoteEmbeddingAuthorizationScope,
  type RemoteEmbeddingOperationPermit,
} from "../authorization/index.js";

const DEFAULT_LOCAL_EMBEDDING = "local/potion-code-16m-v2";

export type DaemonBackendOptions = {
  version: string;
  serviceOptions?: CreateZvecGrepOptions;
  modelPoolOptions?: EmbeddingModelPoolOptions;
  schedulerOptions?: JobSchedulerOptions;
  readSessionIdleTtlMs?: number;
  runtimeIdleTtlMs?: number;
  createService?: typeof createZvecGrep;
  watchManagerFactory?: (options: WatchManagerOptions) => WatchManager;
  logger?: DaemonLogger;
  authorizationStore?: RemoteEmbeddingAuthorizationStore;
  inspectRoot?: typeof inspectRoot;
};

type DaemonIndexInput = ZvecGrepIndexRequest & {
  changedPaths?: readonly string[];
  runtimeOverridesAreEphemeral?: boolean;
  skipInitialStatus?: boolean;
};

export class DaemonBackend implements ZvecGrepDaemonBackend {
  readonly modelPool: EmbeddingModelPool;
  readonly runtimeManager: RuntimeManager;
  readonly scheduler: JobScheduler;
  private readonly startedAt = Date.now();
  private readonly statusCache = new Map<string, ZvecGrepInfoResult>();
  private readonly lastScanDiagnostics = new Map<
    string,
    FileScanDiagnostics | undefined
  >();
  private readonly workspaceRuntimeCache = new Map<
    string,
    EmbeddingRuntimeConfig
  >();
  private readonly watchers = new Map<string, WatchManager>();
  private readonly indexCoordinators = new Map<string, IndexCoordinator>();
  private readonly droppingRoots = new Set<string>();
  private readonly authorizationManager: RemoteEmbeddingAuthorizationManager;
  private shuttingDown = false;
  private closePromise?: Promise<void>;

  constructor(private readonly options: DaemonBackendOptions) {
    this.authorizationManager = new RemoteEmbeddingAuthorizationManager(
      options.authorizationStore ??
        new RemoteEmbeddingAuthorizationStore({
          signingKeyPath: options.serviceOptions?.authorizationSigningKeyPath,
        }),
    );
    this.modelPool = new EmbeddingModelPool({
      ...options.modelPoolOptions,
      serviceOptions: options.serviceOptions,
      logger: options.logger,
    });
    this.runtimeManager = new RuntimeManager({
      modelPool: this.modelPool,
      serviceOptions: options.serviceOptions,
      readSessionIdleTtlMs: options.readSessionIdleTtlMs,
      runtimeIdleTtlMs: options.runtimeIdleTtlMs,
      onRuntimeEvicted: async (root) => {
        this.statusCache.delete(root);
        this.lastScanDiagnostics.delete(root);
        this.workspaceRuntimeCache.delete(root);
        await this.closeWatcher(root);
      },
    });
    this.scheduler = new JobScheduler({
      ...options.schedulerOptions,
      logger: options.logger,
    });
  }

  async planIndexAuthorization(
    input: ZvecGrepIndexRequest,
  ): Promise<RemoteEmbeddingAuthorizationPlan | undefined> {
    if (input.drop === true) {
      return undefined;
    }
    const requestedRoot = await resolveRequestedRoot(input.root, false);
    if (input.rebuild !== true && this.scheduler.hasActiveRoot(requestedRoot)) {
      return undefined;
    }
    const info = await inspectRoot(
      input.root,
      this.options.serviceOptions,
      input.rebuild !== true,
    );
    let modelLoadRequest: EmbeddingModelLoadRequest;
    try {
      modelLoadRequest = this.indexModelLoadRequest(info, input);
    } catch (error) {
      if (error instanceof DaemonError && error.code === "MODEL_LOAD_FAILED") {
        return undefined;
      }
      throw error;
    }
    let lease: ModelLease;
    try {
      lease = await this.modelPool.acquire(modelLoadRequest);
    } catch (error) {
      if (isEngineError(error)) {
        return undefined;
      }
      throw error;
    }
    let schema: WorkspaceIndexEmbeddingSchema;
    let modelInfo: EmbeddingModelInfo;
    try {
      schema = embeddingSchema(lease.model);
      modelInfo = lease.model.info;
    } finally {
      lease.release();
    }
    const existing = info.workspaceIndex?.embedding;
    if (
      existing &&
      input.embedding &&
      input.rebuild !== true &&
      (existing.provider !== schema.provider || existing.model !== schema.model)
    ) {
      throw new DaemonError(
        "EMBEDDING_MODEL_MISMATCH",
        `Existing model ${existing.provider}/${existing.model} does not match requested model ${schema.provider}/${schema.model}; use rebuild to change models.`,
      );
    }
    return await planRemoteIndexAuthorization({
      info,
      model: modelInfo,
      rebuild: input.rebuild,
      store: this.authorizationManager.store,
    });
  }

  async planSearchAuthorization(
    input: NormalizedSearchInput,
  ): Promise<RemoteEmbeddingAuthorizationPlan | undefined> {
    const requestedRoot = await resolveRequestedRoot(input.root, false);
    let activeRuntime = this.runtimeManager.getByRequestedRoot(requestedRoot);
    if (
      activeRuntime?.embeddingProvider() &&
      activeRuntime.embeddingProvider() !== "qwen"
    ) {
      return undefined;
    }
    let canonicalRoot = activeRuntime?.canonicalRoot;
    let discoveredInfo: ZvecGrepInfoResult | undefined;
    if (!canonicalRoot) {
      discoveredInfo = await this.inspectRoot(input.root, false);
      canonicalRoot = await resolveRequestedRoot(discoveredInfo.root, false);
      activeRuntime = this.runtimeManager.getByCanonicalRoot(canonicalRoot);
      const provider =
        activeRuntime?.embeddingProvider() ??
        discoveredInfo.workspaceIndex?.embedding?.provider;
      if (provider && provider !== "qwen") {
        return undefined;
      }
    }
    const cachedInfo = activeRuntime
      ? this.statusCache.get(canonicalRoot)
      : undefined;
    let info: ZvecGrepInfoResult;
    if (cachedInfo) {
      info = cachedInfo;
    } else {
      try {
        info = await this.inspectRoot(canonicalRoot, true);
        this.statusCache.set(canonicalRoot, info);
      } catch (error) {
        const cached = this.statusCache.get(canonicalRoot);
        if (
          !cached ||
          !isEngineError(error) ||
          error.code !== "ZVEC_GREP.ENGINE.LOCK.BUSY"
        ) {
          throw error;
        }
        info = cached;
      }
    }
    const schema = info.workspaceIndex?.embedding;
    if (!info.indexed || !schema || schema.provider !== "qwen") {
      return undefined;
    }
    const modelLoadRequest = this.searchModelLoadRequest(info, input);
    const modelInfo = await this.loadEmbeddingModelInfo(modelLoadRequest);
    return await planRemoteSearchAuthorization({
      info,
      model: modelInfo,
      search: input,
      runtimeNeedsReconciliation:
        this.runtimeManager
          .getByCanonicalRoot(canonicalRoot)
          ?.needsReconciliation() ?? false,
      store: this.authorizationManager.store,
    });
  }

  async existingRemoteEmbeddingPermit(
    plan: RemoteEmbeddingAuthorizationPlan,
  ): Promise<RemoteEmbeddingOperationPermit | undefined> {
    return await this.authorizationManager.existingWorkspacePermit(plan);
  }

  async grantRemoteEmbedding(
    plan: RemoteEmbeddingAuthorizationPlan,
    scope: RemoteEmbeddingAuthorizationScope,
  ): Promise<RemoteEmbeddingOperationPermit> {
    return await this.authorizationManager.grant(plan, scope);
  }

  async index(
    input: ZvecGrepIndexRequest,
    options: {
      authorization?: RemoteEmbeddingOperationPermit;
      onProgress?: (progress: IndexProgress) => void;
    } = {},
  ): Promise<ZvecGrepIndexResult> {
    if (input.drop === true) {
      assertDropOnlyInput(input);
      const result = await this.dropIndex(input);
      return {
        root: result.root,
        jobId: "drop",
        state: "succeeded",
        reused: false,
        action: "drop",
        dropped: result.removed,
      };
    }
    const requestedRoot = await resolveRequestedRoot(input.root, true);
    this.assertRootNotDropping(requestedRoot);
    const runtime = await this.runtimeManager.activateForIndex(requestedRoot);
    this.ensureWatcher(runtime);
    const activeJob = this.scheduler.getByRoot(runtime.canonicalRoot);
    const followsNarrowJob =
      activeJob?.state === "queued" || activeJob?.state === "running"
        ? activeJob.reason === "watch"
        : false;
    const createsWork =
      !this.scheduler.hasActiveRoot(runtime.canonicalRoot) ||
      input.rebuild === true ||
      followsNarrowJob;
    if (createsWork) {
      this.lastScanDiagnostics.delete(runtime.canonicalRoot);
    }
    const targetRevision = createsWork
      ? runtime.markDirty()
      : runtime.snapshot().dirtyRevision;
    const submitted = this.scheduler.submit({
      canonicalRoot: runtime.canonicalRoot,
      reason: "manual",
      followupIfRunning: input.rebuild === true || followsNarrowJob,
      run: async (report, signal) => {
        const proof = await this.runIndex(
          runtime,
          input,
          report,
          options.authorization,
          signal,
        );
        this.lastScanDiagnostics.set(
          runtime.canonicalRoot,
          proof.scanDiagnostics,
        );
        if (proof.reconciled) {
          runtime.markReconciled(targetRevision, proof.reconciliationEpoch);
        } else {
          runtime.markIndexed(targetRevision);
        }
      },
    });
    const job = input.wait
      ? await this.scheduler.wait(submitted.job.id, options.onProgress)
      : submitted.job;
    if (input.wait) {
      await this.settleKnownChanges(runtime);
    }
    this.options.logger?.event("job.submitted", {
      root_id: rootIdentity(runtime.canonicalRoot),
      job_id: job.id,
      reason: "manual",
      state: job.state,
      reused: submitted.reused,
    });
    return {
      root: runtime.canonicalRoot,
      jobId: job.id,
      state: job.state,
      reused: submitted.reused,
      action: "index",
      error: job.error,
      ...(input.debug === true && input.wait === true
        ? {
            scanDiagnostics: this.lastScanDiagnostics.get(
              runtime.canonicalRoot,
            ),
          }
        : {}),
    };
  }

  async dropIndex(
    input: ZvecGrepIndexDropInput,
  ): Promise<ZvecGrepIndexDropResult> {
    const canonicalRoot = await resolveRequestedRoot(input.root, true);
    if (this.droppingRoots.has(canonicalRoot)) {
      throw new DaemonError(
        "INDEX_DROP_IN_PROGRESS",
        "The Workspace index is already being dropped.",
      );
    }
    this.droppingRoots.add(canonicalRoot);
    try {
      await this.closeWatcher(canonicalRoot);
      this.scheduler.cancelRoot(canonicalRoot);
      await this.scheduler.waitForRootIdle(canonicalRoot);
      const runtime = this.runtimeManager.getByCanonicalRoot(canonicalRoot);
      const drop = async (): Promise<boolean> => {
        const service = await (this.options.createService ?? createZvecGrep)({
          ...this.options.serviceOptions,
          root: canonicalRoot,
          daemonInstanceToken: this.runtimeManager.instanceToken,
        });
        try {
          return await service.dropIndex({ root: canonicalRoot });
        } finally {
          await service.close();
        }
      };
      const removed = runtime ? await runtime.withWrite(drop) : await drop();
      this.options.logger?.event("index.dropped", {
        root_id: rootIdentity(canonicalRoot),
        dropped: removed,
      });
      return { root: canonicalRoot, removed };
    } finally {
      this.statusCache.delete(canonicalRoot);
      this.lastScanDiagnostics.delete(canonicalRoot);
      this.workspaceRuntimeCache.delete(canonicalRoot);
      try {
        await this.runtimeManager.evict(canonicalRoot);
      } finally {
        this.droppingRoots.delete(canonicalRoot);
      }
    }
  }

  async search(
    input: NormalizedSearchInput,
    options: { authorization?: RemoteEmbeddingOperationPermit } = {},
  ): Promise<ZvecGrepSearchResult> {
    const startedAt = Date.now();
    const requestedRoot = await resolveRequestedRoot(input.root, false);
    this.assertRootNotDropping(requestedRoot);
    const runtime = await this.runtimeManager.activate(requestedRoot);
    const releaseRuntimeActivity = runtime.beginActivity();
    try {
      const searchInfo = await this.inspectRootWithCache(runtime.canonicalRoot);
      const currentModelLoadRequest = runtime.currentModelLoadRequest();
      const defaultModelLoadRequest = searchInfo.indexed
        ? this.searchModelLoadRequest(searchInfo, {})
        : currentModelLoadRequest;
      if (!defaultModelLoadRequest) {
        throw new DaemonError(
          "INDEX_MISSING",
          "Search requires an existing workspace index.",
        );
      }
      const searchModelLoadRequest = searchInfo.indexed
        ? this.searchModelLoadRequest(searchInfo, input)
        : this.overrideActiveModelLoadRequest(defaultModelLoadRequest, input);
      runtime.updateModelLoadRequest(defaultModelLoadRequest);
      this.ensureWatcher(runtime);
      await runtime.probeInitialFreshness(
        async () => {
          const info = await this.inspectRoot(runtime.canonicalRoot, true);
          this.statusCache.set(runtime.canonicalRoot, info);
          return indexStatusIsFresh(info);
        },
        (initialFreshness) => {
          this.options.logger?.event(
            `runtime.initial_probe_${initialFreshness}`,
            {
              root_id: rootIdentity(runtime.canonicalRoot),
            },
          );
        },
      );
      let updateJob: IndexJobSnapshot | undefined;
      const executeSearch = () =>
        withRemoteEmbeddingOperationPermit(options.authorization, () =>
          runtime.search(
            {
              queries: input.queries,
              routes: input.routes,
              fuse: input.fuse,
              limit: input.limit,
              trace: input.trace,
              preferSymbol: input.preferSymbol,
              symbolTypes: input.symbolTypes,
              globs: normalizePlainStringList(input.globs),
              insensitiveGlobs: normalizePlainStringList(
                input.insensitiveGlobs,
              ),
              fileTypes: normalizePlainStringList(input.fileTypes),
              excludedFileTypes: normalizePlainStringList(
                input.excludedFileTypes,
              ),
              hidden: input.hidden,
              noIgnore: input.noIgnore,
              ignoreFiles: input.ignoreFiles,
              maxDepth: input.maxDepth,
              maxFileSizeBytes: input.maxFileSizeBytes,
              follow: input.follow,
              embeddingConcurrency: input.embeddingConcurrency,
              modifiedAfter: input.modifiedAfter,
              modifiedBefore: input.modifiedBefore,
              autoUpdate: false,
            },
            searchModelLoadRequest,
          ),
        );
      let result;
      if (input.freshness === "wait_for_fresh") {
        while (true) {
          updateJob =
            (await this.waitForFresh(runtime, options.authorization, input)) ??
            updateJob;
          const beforeSearch = runtime.snapshot();
          result = await executeSearch();
          const afterSearch = runtime.snapshot();
          if (
            !afterSearch.watcherPending &&
            afterSearch.watcherEpoch === beforeSearch.watcherEpoch &&
            !runtime.needsReconciliation()
          ) {
            break;
          }
        }
      } else {
        result = await executeSearch();
      }
      if (
        runtime.needsReconciliation() &&
        input.autoUpdate &&
        (runtime.requiresFullReconciliation() ||
          !this.scheduler.hasActiveRoot(runtime.canonicalRoot))
      ) {
        const currentJob = this.scheduler.getByRoot(runtime.canonicalRoot);
        const terminalKnownPathJob =
          !runtime.requiresFullReconciliation() &&
          (currentJob?.state === "failed" || currentJob?.state === "cancelled");
        if (runtime.needsReconciliation() && !terminalKnownPathJob) {
          updateJob = await this.submitIndex(
            runtime,
            {
              root: runtime.canonicalRoot,
              apiKey: input.apiKey,
              device: input.device,
              runtimeOverridesAreEphemeral: true,
            },
            "background_reconcile",
            false,
            options.authorization,
          );
        }
      }
      const job = updateJob ?? this.scheduler.getByRoot(runtime.canonicalRoot);
      const runtimeSnapshot = runtime.snapshot();
      const freshness =
        runtime.needsReconciliation() ||
        runtimeSnapshot.watcherPending ||
        job?.state === "queued" ||
        job?.state === "running"
          ? "possibly_stale"
          : "fresh";
      const response: ZvecGrepSearchResult = {
        root: runtime.canonicalRoot,
        freshness,
        indexing:
          freshness === "possibly_stale"
            ? searchIndexingSnapshot(
                job,
                this.currentIndexCompletion(
                  runtime.canonicalRoot,
                  job,
                  runtime,
                ),
              )
            : undefined,
        result,
      };
      this.options.logger?.event("search.completed", {
        root_id: rootIdentity(runtime.canonicalRoot),
        duration_ms: Date.now() - startedAt,
        freshness: response.freshness,
        result_count: result.items.length,
      });
      return response;
    } finally {
      releaseRuntimeActivity();
    }
  }

  async explore(
    input: ZvecGrepExploreOptions & { root: string },
  ): Promise<ZvecGrepExploreResult> {
    const requestedRoot = await resolveRequestedRoot(input.root, false);
    this.assertRootNotDropping(requestedRoot);
    const runtime = await this.runtimeManager.activate(requestedRoot);
    const release = runtime.beginActivity();
    try {
      return await runtime.explore(input);
    } finally {
      release();
    }
  }

  async graphNeighborhood(
    input: ZvecGrepGraphNeighborhoodOptions & { root: string },
  ): Promise<ZvecGrepGraphNeighborhoodResult> {
    const requestedRoot = await resolveRequestedRoot(input.root, false);
    this.assertRootNotDropping(requestedRoot);
    const runtime = await this.runtimeManager.activate(requestedRoot);
    const release = runtime.beginActivity();
    try {
      return await runtime.graphNeighborhood(input);
    } finally {
      release();
    }
  }

  private currentIndexCompletion(
    canonicalRoot: string,
    job: IndexJobSnapshot | undefined,
    runtime: RootRuntime,
  ) {
    // Narrow watcher updates skip the status scan, so the cached completion
    // still describes the previous fresh snapshot rather than the active job.
    if (
      job?.reason === "watch" &&
      runtime.needsReconciliation() &&
      !runtime.requiresFullReconciliation()
    ) {
      return undefined;
    }
    return indexCompletionFromStatus(
      this.statusCache.get(canonicalRoot)?.status,
    );
  }

  async indexStatus(
    input: ZvecGrepIndexStatusInput,
  ): Promise<ZvecGrepIndexStatusResult> {
    const requestedCanonicalRoot = await resolveRequestedRoot(
      input.root,
      false,
    );
    let info: ZvecGrepInfoResult;
    try {
      info = await inspectRoot(input.root, this.options.serviceOptions);
      this.statusCache.set(requestedCanonicalRoot, info);
    } catch (error) {
      const cached = this.statusCache.get(requestedCanonicalRoot);
      if (
        !cached ||
        !isEngineError(error) ||
        error.code !== "ZVEC_GREP.ENGINE.LOCK.BUSY"
      ) {
        throw error;
      }
      info = cached;
    }
    const canonicalRoot = await resolveRequestedRoot(info.root, false);
    const runtime = this.runtimeManager.getByCanonicalRoot(canonicalRoot);
    const runtimeSnapshot = runtime?.snapshot();
    const job = this.scheduler.getByRoot(canonicalRoot);
    return {
      root: canonicalRoot,
      indexed: info.indexed,
      indexPolicy: info.indexPolicy,
      source: info.source,
      persistent: persistentStatus(info),
      runtime: runtimeSnapshot
        ? {
            watcherActive: runtimeSnapshot.watcherActive,
            dirtyRevision: runtimeSnapshot.dirtyRevision,
            indexedRevision: runtimeSnapshot.indexedRevision,
            activeJobId: job?.id,
            jobState: job?.state,
            progress: job?.progress ? formatProgress(job) : undefined,
            completion: indexCompletionForJob(
              indexCompletionFromStatus(info.status),
              job?.state,
              job?.progress,
            ),
            error: job?.error,
          }
        : undefined,
    };
  }

  async serverStatus(): Promise<ZvecGrepServerStatusResult> {
    const runtime = this.runtimeManager.snapshot();
    const models = this.modelPool.snapshot();
    const jobs = this.scheduler.snapshot();
    return {
      version: this.options.version,
      uptimeMs: Date.now() - this.startedAt,
      shuttingDown: this.shuttingDown,
      activeRuntimes: runtime.activeRuntimes,
      queuedJobs: jobs.queued,
      runningJobs: jobs.running,
      models,
    };
  }

  async rg(input: ZvecGrepRgInput): Promise<ZvecGrepRgResult> {
    const startedAt = Date.now();
    const canonicalRoot = await resolveRequestedRoot(input.root, false);
    const service = await (this.options.createService ?? createZvecGrep)({
      ...this.options.serviceOptions,
      root: canonicalRoot,
      daemonInstanceToken: this.runtimeManager.instanceToken,
    });
    try {
      const result = await service.context({
        ...contextOptionsFromRgInput({ ...input, root: canonicalRoot }),
        root: canonicalRoot,
        autoUpdate: false,
      });
      this.options.logger?.event("rg.completed", {
        root_id: rootIdentity(canonicalRoot),
        duration_ms: Date.now() - startedAt,
        result_count: result.items.length,
      });
      return {
        root: canonicalRoot,
        result,
      };
    } finally {
      await service.close();
    }
  }

  async close(): Promise<void> {
    if (this.closePromise) {
      return this.closePromise;
    }
    this.shuttingDown = true;
    this.closePromise = (async () => {
      await Promise.all(
        [...this.watchers.values()].map((watcher) => watcher.close()),
      );
      for (const root of this.watchers.keys()) {
        this.runtimeManager.getByCanonicalRoot(root)?.setWatcherActive(false);
      }
      this.watchers.clear();
      this.indexCoordinators.clear();
      await this.scheduler.close();
      await this.runtimeManager.close();
      await this.modelPool.close();
      this.lastScanDiagnostics.clear();
      this.workspaceRuntimeCache.clear();
    })();
    return this.closePromise;
  }

  private async runIndex(
    runtime: RootRuntime,
    input: DaemonIndexInput,
    report: (progress: IndexProgress) => void,
    authorization?: RemoteEmbeddingOperationPermit,
    signal?: AbortSignal,
  ): Promise<IndexReconciliationProof> {
    const releaseRuntimeActivity = runtime.beginActivity();
    try {
      return await this.runIndexOperation(
        runtime,
        input,
        report,
        authorization,
        signal,
      );
    } finally {
      releaseRuntimeActivity();
    }
  }

  private async runIndexOperation(
    runtime: RootRuntime,
    input: DaemonIndexInput,
    report: (progress: IndexProgress) => void,
    authorization?: RemoteEmbeddingOperationPermit,
    signal?: AbortSignal,
  ): Promise<IndexReconciliationProof> {
    const startedAt = Date.now();
    // Watcher path updates already identify their indexing scope. Uncertain
    // watcher reconciliations are verified by the final status scan, so neither
    // path needs another workspace-wide status scan before indexing starts.
    const includeInitialStatus =
      input.skipInitialStatus !== true &&
      input.rebuild !== true &&
      input.changedPaths === undefined;
    const includeFinalStatus = input.changedPaths === undefined;
    const before = await this.inspectRoot(
      runtime.canonicalRoot,
      includeInitialStatus,
    );
    if (includeInitialStatus) {
      this.statusCache.set(runtime.canonicalRoot, before);
    }
    const modelLoadRequest = this.indexModelLoadRequest(before, input);
    const model = modelLoadRequest.model;
    runtime.updateModelLoadRequest(modelLoadRequest);
    let lease: ModelLease;
    try {
      lease = await this.modelPool.acquire(modelLoadRequest);
    } catch (error) {
      throw new DaemonError(
        "MODEL_LOAD_FAILED",
        `Embedding model ${embeddingModelReference(model)} could not be created: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
    let service: Awaited<ReturnType<typeof createZvecGrep>> | undefined;
    let scanDiagnostics: FileScanDiagnostics | undefined;
    try {
      service = await (this.options.createService ?? createZvecGrep)({
        ...this.options.serviceOptions,
        root: runtime.canonicalRoot,
        embeddingModel: lease.model,
        embeddingModelOwnership: "borrowed",
        embedding: input.runtimeOverridesAreEphemeral
          ? undefined
          : input.embedding,
        apiKey: input.runtimeOverridesAreEphemeral ? undefined : input.apiKey,
        endpoint: input.runtimeOverridesAreEphemeral
          ? undefined
          : input.endpoint,
        device: input.runtimeOverridesAreEphemeral ? undefined : input.device,
        daemonInstanceToken: this.runtimeManager.instanceToken,
      });
      const result = await runtime.withWrite(() =>
        withRemoteEmbeddingOperationPermit(authorization, () =>
          service!.index({
            root: runtime.canonicalRoot,
            rebuild: input.rebuild,
            resetPaths: input.resetPaths,
            globs: normalizePlainStringList(input.globs),
            insensitiveGlobs: normalizePlainStringList(input.insensitiveGlobs),
            fileTypes: normalizePlainStringList(input.fileTypes),
            excludedFileTypes: normalizePlainStringList(
              input.excludedFileTypes,
            ),
            hidden: input.hidden,
            noIgnore: input.noIgnore,
            ignoreFiles: normalizePlainStringList(input.ignoreFiles),
            maxDepth: input.maxDepth,
            maxFileSizeBytes: input.maxFileSizeBytes,
            follow: input.follow,
            embeddingConcurrency: input.embeddingConcurrency,
            changedPaths: input.changedPaths,
            signal,
            onProgress: report,
            onWriterContext: (context) =>
              runtime.setWriterContext(context, lease.key),
          }),
        ),
      );
      scanDiagnostics = result.scanDiagnostics;
    } finally {
      try {
        await service?.close();
      } finally {
        lease.release();
      }
    }

    if (includeFinalStatus) {
      await this.watchers.get(runtime.canonicalRoot)?.flushPending();
    }
    const proofReconciliationEpoch = runtime.reconciliationEpoch();
    const after = await this.inspectRoot(
      runtime.canonicalRoot,
      includeFinalStatus,
    );
    if (includeFinalStatus) {
      this.statusCache.set(runtime.canonicalRoot, after);
      await this.watchers.get(runtime.canonicalRoot)?.refreshPaths?.();
    }
    if (!after.workspaceIndex?.embedding) {
      throw new DaemonError(
        "INDEX_MISSING",
        "Index completed without an embedding schema.",
      );
    }
    runtime.updateModelLoadRequest(this.searchModelLoadRequest(after, {}));
    this.options.logger?.event("index.completed", {
      root_id: rootIdentity(runtime.canonicalRoot),
      duration_ms: Date.now() - startedAt,
      scope: input.changedPaths ? "paths" : "reconcile",
      changed_paths: input.changedPaths?.length,
    });
    const reconciled = includeFinalStatus && indexStatusIsFresh(after);
    if (includeFinalStatus && !reconciled) {
      runtime.requireFullReconciliation(true);
    }
    return {
      reconciled,
      reconciliationEpoch: proofReconciliationEpoch,
      scanDiagnostics,
    };
  }

  private async submitIndex(
    runtime: RootRuntime,
    input: DaemonIndexInput,
    reason: "background_reconcile" | "fresh_query",
    wait: boolean,
    authorization?: RemoteEmbeddingOperationPermit,
  ): Promise<IndexJobSnapshot> {
    const createsWork =
      !this.scheduler.hasActiveRoot(runtime.canonicalRoot) ||
      input.rebuild === true;
    const targetRevision = createsWork
      ? runtime.markDirty()
      : runtime.snapshot().dirtyRevision;
    const followsNarrowWatch =
      this.scheduler.getByRoot(runtime.canonicalRoot)?.reason === "watch";
    const submitted = this.scheduler.submit({
      canonicalRoot: runtime.canonicalRoot,
      reason,
      followupIfRunning: followsNarrowWatch,
      run: async (report, signal) => {
        if (
          reason === "background_reconcile" &&
          runtime.canProbeFullReconciliation() &&
          (await this.probeCurrentFreshness(runtime)) === "fresh"
        ) {
          return;
        }
        const proof = await this.runIndex(
          runtime,
          input,
          report,
          authorization,
          signal,
        );
        if (proof.reconciled) {
          runtime.markReconciled(targetRevision, proof.reconciliationEpoch);
        } else {
          runtime.markIndexed(targetRevision);
        }
      },
    });
    if (!wait) return submitted.job;
    const job = await this.scheduler.wait(submitted.job.id);
    await this.scheduler.waitForRootIdle(runtime.canonicalRoot);
    return job;
  }

  private ensureWatcher(runtime: RootRuntime): void {
    if (this.watchers.has(runtime.canonicalRoot) || this.shuttingDown) {
      return;
    }
    const coordinator = new IndexCoordinator({
      runtime,
      scheduler: this.scheduler,
      run: async (changes, report, signal) => {
        const changedPaths = [
          ...changes.touchedFiles,
          ...changes.rescanDirectories,
          ...changes.deletedPrefixes,
        ];
        if (!changes.forceFullReconcile && changedPaths.length === 0) {
          return;
        }
        const automaticAuthorization =
          await this.automaticIndexAuthorization(runtime);
        if (!automaticAuthorization.allowed) {
          throw new DaemonError(
            "REMOTE_EMBEDDING_AUTH_REQUIRED",
            "A Workspace Remote Embedding grant is required for file-watcher index updates.",
          );
        }
        return await this.runIndex(
          runtime,
          {
            root: runtime.canonicalRoot,
            changedPaths: changes.forceFullReconcile ? undefined : changedPaths,
            skipInitialStatus: changes.forceFullReconcile,
          },
          report,
          automaticAuthorization.authorization,
          signal,
        );
      },
    });
    const watcher = (
      this.options.watchManagerFactory ??
      ((options) => new WatchManager(options))
    )({
      root: runtime.canonicalRoot,
      onChanges: async (changes, reason) => {
        const pathCount =
          changes.touchedFiles.length +
          changes.rescanDirectories.length +
          changes.deletedPrefixes.length;
        if (changes.forceFullReconcile && pathCount === 0) {
          runtime.requireFullReconciliation(true);
          this.options.logger?.event("watcher.reconciliation_probe_requested", {
            root_id: rootIdentity(runtime.canonicalRoot),
            reason,
            active_job: this.scheduler.hasActiveRoot(runtime.canonicalRoot),
          });
          return;
        }
        const automaticAuthorization =
          await this.automaticIndexAuthorization(runtime);
        if (!automaticAuthorization.allowed) {
          runtime.markDirty();
          runtime.requireFullReconciliation();
          this.options.logger?.event("watcher.authorization_required", {
            root_id: rootIdentity(runtime.canonicalRoot),
            reason,
          });
          return;
        }
        this.options.logger?.event(
          changes.forceFullReconcile ? "watcher.overflow" : "watcher.changes",
          {
            root_id: rootIdentity(runtime.canonicalRoot),
            reason,
            touched_files: changes.touchedFiles.length,
            rescan_directories: changes.rescanDirectories.length,
            deleted_prefixes: changes.deletedPrefixes.length,
          },
        );
        coordinator.enqueue(changes, reason);
      },
      onPendingChange: (pending) => runtime.setWatcherPending(pending),
      getRootPaths: () =>
        this.statusCache.get(runtime.canonicalRoot)?.workspaceIndex?.rootPaths,
    });
    watcher.start();
    this.indexCoordinators.set(runtime.canonicalRoot, coordinator);
    this.watchers.set(runtime.canonicalRoot, watcher);
    runtime.setWatcherActive(true);
  }

  private async settleKnownChanges(runtime: RootRuntime): Promise<void> {
    const watcher = this.watchers.get(runtime.canonicalRoot);
    while (true) {
      await watcher?.flushPending();
      await this.scheduler.waitForRootIdle(runtime.canonicalRoot);
      const job = this.scheduler.getByRoot(runtime.canonicalRoot);
      const snapshot = runtime.snapshot();
      if (
        job?.state === "failed" ||
        job?.state === "cancelled" ||
        !snapshot.watcherPending
      ) {
        return;
      }
    }
  }

  private async automaticIndexAuthorization(runtime: RootRuntime): Promise<{
    allowed: boolean;
    authorization?: RemoteEmbeddingOperationPermit;
  }> {
    const knownProvider = runtime.embeddingProvider();
    if (knownProvider && knownProvider !== "qwen") return { allowed: true };
    const root = runtime.canonicalRoot;
    const info = await this.inspectRoot(root, false);
    const schema = info.workspaceIndex?.embedding;
    if (!schema || schema.provider !== "qwen") return { allowed: true };
    const modelInfo = await this.loadEmbeddingModelInfo(
      this.searchModelLoadRequest(info, {}),
    );
    const plan = await planRemoteIndexAuthorization({
      info,
      model: modelInfo,
      needsUpdate: true,
      store: this.authorizationManager.store,
    });
    if (!plan) return { allowed: true };
    const authorization =
      await this.authorizationManager.existingWorkspacePermit(plan);
    return authorization
      ? { allowed: true, authorization }
      : { allowed: false };
  }

  private async waitForFresh(
    runtime: RootRuntime,
    authorization?: RemoteEmbeddingOperationPermit,
    runtimeOverrides: Pick<NormalizedSearchInput, "apiKey" | "device"> = {},
  ): Promise<IndexJobSnapshot | undefined> {
    let updateJob: IndexJobSnapshot | undefined;
    while (true) {
      await this.settleKnownChanges(runtime);
      if (!runtime.needsReconciliation()) {
        return updateJob;
      }
      if (
        !runtime.requiresFullReconciliation() ||
        runtime.canProbeFullReconciliation() ||
        this.scheduler.getByRoot(runtime.canonicalRoot)?.state === "failed" ||
        this.scheduler.getByRoot(runtime.canonicalRoot)?.state === "cancelled"
      ) {
        const freshness = await this.probeCurrentFreshness(runtime);
        if (freshness === "fresh") {
          return updateJob;
        }
      }
      const currentJob = this.scheduler.getByRoot(runtime.canonicalRoot);
      if (
        !runtime.requiresFullReconciliation() &&
        (currentJob?.state === "failed" || currentJob?.state === "cancelled")
      ) {
        throw new DaemonError(
          currentJob.error?.code ?? "INDEX_FAILED",
          currentJob.error?.message ??
            "Known index changes did not complete successfully.",
        );
      }
      updateJob = await this.submitIndex(
        runtime,
        {
          root: runtime.canonicalRoot,
          apiKey: runtimeOverrides.apiKey,
          device: runtimeOverrides.device,
          runtimeOverridesAreEphemeral: true,
        },
        "fresh_query",
        true,
        authorization,
      );
      if (updateJob.state !== "succeeded") {
        throw new DaemonError(
          updateJob.error?.code ?? "INDEX_FAILED",
          updateJob.error?.message ??
            "Index reconciliation did not complete successfully.",
        );
      }
    }
  }

  private async loadEmbeddingModelInfo(
    request: EmbeddingModelLoadRequest,
  ): Promise<EmbeddingModelInfo> {
    const lease = await this.modelPool.acquire(request);
    try {
      return lease.model.info;
    } finally {
      lease.release();
    }
  }

  private async probeCurrentFreshness(
    runtime: RootRuntime,
  ): Promise<"fresh" | "stale"> {
    const releaseRuntimeActivity = runtime.beginActivity();
    try {
      const freshness = await runtime.probeFreshness(async () => {
        const info = await this.inspectRoot(runtime.canonicalRoot, true);
        this.statusCache.set(runtime.canonicalRoot, info);
        return indexStatusIsFresh(info);
      });
      this.options.logger?.event(`runtime.recovery_probe_${freshness}`, {
        root_id: rootIdentity(runtime.canonicalRoot),
      });
      return freshness;
    } finally {
      releaseRuntimeActivity();
    }
  }

  private async closeWatcher(canonicalRoot: string): Promise<void> {
    const watcher = this.watchers.get(canonicalRoot);
    this.watchers.delete(canonicalRoot);
    this.indexCoordinators.delete(canonicalRoot);
    await watcher?.close();
  }

  private assertRootNotDropping(canonicalRoot: string): void {
    if (this.droppingRoots.has(canonicalRoot)) {
      throw new DaemonError(
        "INDEX_DROP_IN_PROGRESS",
        "The Workspace index is being dropped.",
        true,
      );
    }
  }

  private indexModel(
    info: ZvecGrepInfoResult,
    input: Pick<ZvecGrepIndexRequest, "embedding" | "embeddingEnvironment">,
  ): EmbeddingModelLoadRequest["model"] {
    if (info.workspaceIndex?.embedding && !input.embedding) {
      return {
        provider: info.workspaceIndex.embedding.provider,
        name: info.workspaceIndex.embedding.model,
      };
    }
    const reference = resolveEmbeddingReference({
      explicit: input.embedding ?? this.options.serviceOptions?.embedding,
      environment: {
        ZVEC_GREP_EMBEDDING:
          input.embeddingEnvironment ?? process.env.ZVEC_GREP_EMBEDDING,
      },
      globalDefault: readGlobalConfig().defaults?.embedding,
      fallback: DEFAULT_LOCAL_EMBEDDING,
    });
    if (!reference) {
      throw new DaemonError(
        "MODEL_LOAD_FAILED",
        "A new index requires embedding or an explicit server default model.",
      );
    }
    return parseEmbeddingModelReference(reference);
  }

  private indexModelLoadRequest(
    info: ZvecGrepInfoResult,
    input: Pick<
      DaemonIndexInput,
      | "embedding"
      | "embeddingEnvironment"
      | "apiKey"
      | "endpoint"
      | "device"
      | "rebuild"
    >,
  ): EmbeddingModelLoadRequest {
    const model = this.indexModel(info, input);
    const workspaceRuntime =
      info.workspaceIndex?.embedding?.provider === model.provider
        ? this.readWorkspaceEmbeddingRuntime(info)
        : {};
    const runtime = this.resolveModelRuntime(model, workspaceRuntime, input);
    if (
      input.rebuild !== true &&
      info.workspaceIndex?.embedding &&
      workspaceRuntime.endpoint !== runtime.endpoint
    ) {
      throw new DaemonError(
        "EMBEDDING_ENDPOINT_MISMATCH",
        "The requested embedding endpoint differs from the workspace snapshot; use rebuild to change endpoints.",
      );
    }
    return { model, runtime };
  }

  private searchModelLoadRequest(
    info: ZvecGrepInfoResult,
    overrides: Pick<NormalizedSearchInput, "apiKey" | "device">,
  ): EmbeddingModelLoadRequest {
    const schema = info.workspaceIndex?.embedding;
    if (!info.indexed || !schema) {
      throw new DaemonError(
        "INDEX_MISSING",
        "Search requires an existing workspace index.",
      );
    }
    const model = {
      provider: schema.provider,
      name: schema.model,
    };
    const workspaceRuntime = this.readWorkspaceEmbeddingRuntime(info);
    const runtime = this.resolveModelRuntime(
      model,
      workspaceRuntime,
      overrides,
    );
    return { model, runtime };
  }

  private resolveModelRuntime(
    model: EmbeddingModelLoadRequest["model"],
    workspaceRuntime: EmbeddingRuntimeConfig,
    overrides: {
      apiKey?: string;
      endpoint?: string;
      device?: "auto" | "cpu" | "metal" | "vulkan" | "cuda";
    },
  ): EmbeddingRuntimeConfig {
    const serviceOptions = this.options.serviceOptions;
    return resolveEmbeddingRuntimeOptions(
      embeddingModelReference(model),
      {
        apiKey: overrides.apiKey ?? serviceOptions?.apiKey,
        endpoint: overrides.endpoint ?? serviceOptions?.endpoint,
        device: overrides.device ?? serviceOptions?.device,
      },
      workspaceRuntime,
      readGlobalConfig(),
    );
  }

  private overrideActiveModelLoadRequest(
    request: EmbeddingModelLoadRequest,
    overrides: Pick<NormalizedSearchInput, "apiKey" | "device">,
  ): EmbeddingModelLoadRequest {
    const runtime = resolveEmbeddingRuntimeOptions(
      embeddingModelReference(request.model),
      overrides,
      request.runtime ?? {},
      readGlobalConfig(),
    );
    return { model: request.model, runtime };
  }

  private readWorkspaceEmbeddingRuntime(
    info: ZvecGrepInfoResult,
  ): EmbeddingRuntimeConfig {
    try {
      const runtime = readWorkspaceEmbeddingRuntime(info);
      this.workspaceRuntimeCache.set(info.root, runtime);
      return runtime;
    } catch (error) {
      const cached = this.workspaceRuntimeCache.get(info.root);
      if (
        cached &&
        isEngineError(error) &&
        (error.code === "ZVEC_GREP.ENGINE.STORAGE.ZVEC_OPEN_FAILED" ||
          error.code === "ZVEC_GREP.ENGINE.LOCK.BUSY")
      ) {
        return cached;
      }
      throw error;
    }
  }

  private async inspectRootWithCache(
    root: string,
  ): Promise<ZvecGrepInfoResult> {
    const cached = this.statusCache.get(root);
    if (cached) {
      return cached;
    }
    try {
      const info = await this.inspectRoot(root, false);
      this.statusCache.set(root, info);
      return info;
    } catch (error) {
      const fallback = this.statusCache.get(root);
      if (
        fallback &&
        isEngineError(error) &&
        error.code === "ZVEC_GREP.ENGINE.LOCK.BUSY"
      ) {
        return fallback;
      }
      throw error;
    }
  }

  private async inspectRoot(
    root: string,
    includeStatus: boolean,
  ): Promise<ZvecGrepInfoResult> {
    return await (this.options.inspectRoot ?? inspectRoot)(
      root,
      this.options.serviceOptions,
      includeStatus,
    );
  }
}

function readWorkspaceEmbeddingRuntime(
  info: ZvecGrepInfoResult,
): EmbeddingRuntimeConfig {
  if (!info.workspaceIndex) return {};
  const location = workspaceIndexLocation(info.root);
  return readWorkspaceManifest(location.home)?.embeddingRuntime ?? {};
}

function indexStatusIsFresh(info: ZvecGrepInfoResult): boolean {
  const status = info.status;
  return (
    status !== null &&
    status !== undefined &&
    status.filesAdded === 0 &&
    status.filesModified === 0 &&
    status.filesDeleted === 0 &&
    status.filesPending === 0 &&
    status.filesFailed === 0
  );
}

function assertDropOnlyInput(input: ZvecGrepIndexInput): void {
  const conflicts: Array<[boolean, string]> = [
    [input.embedding !== undefined, "embedding"],
    [input.apiKey !== undefined, "apiKey"],
    [input.endpoint !== undefined, "endpoint"],
    [input.device !== undefined, "device"],
    [input.rebuild !== undefined, "rebuild"],
    [input.resetPaths !== undefined, "resetPaths"],
    [input.globs !== undefined, "globs"],
    [input.insensitiveGlobs !== undefined, "insensitiveGlobs"],
    [input.fileTypes !== undefined, "fileTypes"],
    [input.excludedFileTypes !== undefined, "excludedFileTypes"],
    [input.hidden !== undefined, "hidden"],
    [input.noIgnore !== undefined, "noIgnore"],
    [input.ignoreFiles !== undefined, "ignoreFiles"],
    [input.maxDepth !== undefined, "maxDepth"],
    [input.maxFileSizeBytes !== undefined, "maxFileSizeBytes"],
    [input.follow !== undefined, "follow"],
    [input.embeddingConcurrency !== undefined, "embeddingConcurrency"],
    [input.wait !== undefined, "wait"],
  ];
  const names = conflicts
    .filter(([conflictsWithDrop]) => conflictsWithDrop)
    .map(([, name]) => name);
  if (names.length > 0) {
    throw new DaemonError(
      "INVALID_ARGUMENT",
      `zvec_grep_index drop cannot be combined with ${names.join(", ")}.`,
    );
  }
}

function embeddingSchema(model: EmbeddingModel): WorkspaceIndexEmbeddingSchema {
  return {
    provider: model.info.provider,
    model: model.info.name,
    dimension: model.info.dimension,
    metric: model.info.metric,
  };
}

function parseEmbeddingModelReference(
  reference: string,
): EmbeddingModelLoadRequest["model"] {
  const separator = reference.indexOf("/");
  if (separator <= 0 || separator === reference.length - 1) {
    throw new DaemonError(
      "MODEL_LOAD_FAILED",
      `Embedding model reference ${reference} is invalid.`,
    );
  }
  return {
    provider: reference.slice(0, separator),
    name: reference.slice(separator + 1),
  };
}

function embeddingModelReference(
  model: EmbeddingModelLoadRequest["model"],
): string {
  return `${model.provider}/${model.name}`;
}

function persistentStatus(
  info: ZvecGrepInfoResult,
): ZvecGrepIndexStatusResult["persistent"] {
  return {
    home: info.home,
    index_path: info.indexPath,
    workspace_index: info.workspaceIndex
      ? {
          id: info.workspaceIndex.id,
          name: info.workspaceIndex.name,
          path: info.workspaceIndex.path,
          root_paths: info.workspaceIndex.rootPaths.map((rootPath) => ({
            absolute_path: rootPath.absolutePath,
            recursive: rootPath.recursive,
            include: rootPath.include ? [...rootPath.include] : undefined,
            exclude: rootPath.exclude ? [...rootPath.exclude] : undefined,
            globs: rootPath.globs ? [...rootPath.globs] : undefined,
            insensitive_globs: rootPath.insensitiveGlobs
              ? [...rootPath.insensitiveGlobs]
              : undefined,
            file_types: rootPath.fileTypes
              ? [...rootPath.fileTypes]
              : undefined,
            excluded_file_types: rootPath.excludedFileTypes
              ? [...rootPath.excludedFileTypes]
              : undefined,
            hidden: rootPath.hidden,
            no_ignore: rootPath.noIgnore,
            ignore_files: rootPath.ignoreFiles
              ? [...rootPath.ignoreFiles]
              : undefined,
            max_depth: rootPath.maxDepth,
            max_file_size_bytes: rootPath.maxFileSizeBytes,
            follow: rootPath.follow,
          })),
          embedding: info.workspaceIndex.embedding,
          index_version: info.workspaceIndex.indexVersion,
          created_time: info.workspaceIndex.createdTime,
          updated_time: info.workspaceIndex.updatedTime,
        }
      : undefined,
    files: info.status
      ? {
          stored: info.status.filesStored,
          scanned: info.status.filesScanned,
          indexed: info.status.filesIndexed,
          pending: info.status.filesPending,
          failed: info.status.filesFailed,
          added: info.status.filesAdded,
          modified: info.status.filesModified,
          deleted: info.status.filesDeleted,
          unchanged: info.status.filesUnchanged,
          entities: info.status.entitiesIndexed,
          truncated_fragments: info.status.fragmentsTruncated,
        }
      : undefined,
    suggestion: info.suggestion,
  };
}

function formatProgress(
  job: IndexJobSnapshot,
):
  | NonNullable<NonNullable<ZvecGrepIndexStatusResult["runtime"]>["progress"]>
  | undefined {
  const progress = job.progress;
  if (!progress) {
    return undefined;
  }
  return {
    phase: progress.phase,
    files_total: progress.filesTotal,
    files_indexed: progress.filesIndexed,
    files_failed: progress.filesFailed,
    detail: progress.detail,
  };
}

function searchIndexingSnapshot(
  job: IndexJobSnapshot | undefined,
  completion: ReturnType<typeof indexCompletionFromStatus>,
): NonNullable<ZvecGrepSearchResult["indexing"]> {
  const state = !job || job.state === "succeeded" ? "idle" : job.state;
  const effectiveCompletion = indexCompletionForJob(
    completion,
    job?.state,
    job?.progress,
  );
  return {
    state,
    ...effectiveCompletion,
  };
}
