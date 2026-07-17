import { createZvecGrep } from "../engine/service/index.js";
import type {
  CreateZvecGrepOptions,
  ZvecGrepInfoResult,
} from "../engine/service/types.js";
import { getEmbeddingModelCatalogEntry } from "../engine/models/index.js";
import { isEngineError } from "../engine/errors/index.js";
import type {
  CollectionEmbeddingSchema,
  IndexProgress,
} from "../engine/types.js";
import {
  contextOptionsFromRgInput,
  normalizePlainStringList,
  type NormalizedSearchInput,
} from "../mcp/input-normalization.js";
import type {
  ZvecGrepDaemonBackend,
  ZvecGrepIndexResult,
  ZvecGrepIndexStatusResult,
  ZvecGrepRgResult,
  ZvecGrepSearchResult,
  ZvecGrepServerStatusResult,
} from "../mcp/tools.js";
import type {
  ZvecGrepIndexInput,
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

const DEFAULT_LOCAL_EMBEDDING = "local/embeddinggemma-300m";

export type DaemonBackendOptions = {
  version: string;
  serviceOptions?: CreateZvecGrepOptions;
  modelPoolOptions?: EmbeddingModelPoolOptions;
  schedulerOptions?: JobSchedulerOptions;
  readCollectionIdleTtlMs?: number;
  runtimeIdleTtlMs?: number;
  resolveEmbeddingSchema?: (reference: string) => CollectionEmbeddingSchema;
  createService?: typeof createZvecGrep;
  watchManagerFactory?: (options: WatchManagerOptions) => WatchManager;
  logger?: DaemonLogger;
};

type DaemonIndexInput = ZvecGrepIndexInput & {
  changedPaths?: readonly string[];
};

export class DaemonBackend implements ZvecGrepDaemonBackend {
  readonly modelPool: EmbeddingModelPool;
  readonly runtimeManager: RuntimeManager;
  readonly scheduler: JobScheduler;
  private readonly startedAt = Date.now();
  private readonly statusCache = new Map<string, ZvecGrepInfoResult>();
  private readonly watchers = new Map<string, WatchManager>();
  private readonly indexCoordinators = new Map<string, IndexCoordinator>();
  private shuttingDown = false;
  private closePromise?: Promise<void>;

  constructor(private readonly options: DaemonBackendOptions) {
    this.modelPool = new EmbeddingModelPool({
      ...options.modelPoolOptions,
      serviceOptions: options.serviceOptions,
      logger: options.logger,
    });
    this.runtimeManager = new RuntimeManager({
      modelPool: this.modelPool,
      serviceOptions: options.serviceOptions,
      readCollectionIdleTtlMs: options.readCollectionIdleTtlMs,
      runtimeIdleTtlMs: options.runtimeIdleTtlMs,
      onRuntimeEvicted: (root) => this.closeWatcher(root),
    });
    this.scheduler = new JobScheduler({
      ...options.schedulerOptions,
      logger: options.logger,
    });
  }

  async index(input: ZvecGrepIndexInput): Promise<ZvecGrepIndexResult> {
    if (input.drop === true) {
      return await this.dropIndex(input);
    }
    const runtime = await this.runtimeManager.activateForIndex(input.root);
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
    const targetRevision = createsWork
      ? runtime.markDirty()
      : runtime.snapshot().dirtyRevision;
    runtime.setWriterPending(true);
    let submitted;
    try {
      submitted = this.scheduler.submit({
        canonicalRoot: runtime.canonicalRoot,
        reason: "manual",
        followupIfRunning: input.rebuild === true || followsNarrowJob,
        run: (report) =>
          runtime.withWrite(async () => {
            const proof = await this.runIndex(runtime, input, report);
            if (proof.reconciled) {
              runtime.markReconciled(targetRevision, proof.reconciliationEpoch);
            } else {
              runtime.markIndexed(targetRevision);
            }
          }),
      });
    } catch (error) {
      runtime.setWriterPending(false);
      throw error;
    }

    if (!submitted.reused) {
      void this.scheduler.waitForRootIdle(runtime.canonicalRoot).finally(() => {
        runtime.setWriterPending(false);
      });
    }
    const job = input.wait
      ? await this.scheduler.wait(submitted.job.id)
      : submitted.job;
    if (input.wait) {
      await this.settleKnownChanges(runtime);
      runtime.setWriterPending(false);
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
    };
  }

  async search(input: NormalizedSearchInput): Promise<ZvecGrepSearchResult> {
    const startedAt = Date.now();
    const runtime = await this.runtimeManager.activate(input.root);
    this.ensureWatcher(runtime);
    await runtime.probeInitialFreshness(
      async () => {
        const info = await inspectRoot(
          runtime.canonicalRoot,
          this.options.serviceOptions,
        );
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
      runtime.search({
        queries: input.queries,
        routes: input.routes,
        fuse: input.fuse,
        limit: input.limit,
        trace: input.trace,
        preferSymbol: input.preferSymbol,
        symbolTypes: input.symbolTypes,
        includePaths: input.includePaths,
        excludePaths: input.excludePaths,
        globs: normalizePlainStringList(input.globs),
        insensitiveGlobs: normalizePlainStringList(input.insensitiveGlobs),
        fileTypes: normalizePlainStringList(input.fileTypes),
        excludedFileTypes: normalizePlainStringList(input.excludedFileTypes),
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
      });
    let result;
    if (input.freshness === "wait_for_fresh") {
      while (true) {
        updateJob = (await this.waitForFresh(runtime)) ?? updateJob;
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
      if (
        !runtime.requiresFullReconciliation() ||
        runtime.canProbeFullReconciliation()
      ) {
        await this.probeCurrentFreshness(runtime);
      }
      const currentJob = this.scheduler.getByRoot(runtime.canonicalRoot);
      const terminalKnownPathJob =
        !runtime.requiresFullReconciliation() &&
        (currentJob?.state === "failed" || currentJob?.state === "cancelled");
      if (runtime.needsReconciliation() && !terminalKnownPathJob) {
        updateJob = await this.submitIndex(
          runtime,
          { root: runtime.canonicalRoot },
          "background_reconcile",
          false,
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
          ? searchIndexingSnapshot(job)
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
    })();
    return this.closePromise;
  }

  private async runIndex(
    runtime: RootRuntime,
    input: DaemonIndexInput,
    report: (progress: IndexProgress) => void,
  ): Promise<IndexReconciliationProof> {
    const startedAt = Date.now();
    const includeStatus = !input.changedPaths;
    const before = await inspectRoot(
      runtime.canonicalRoot,
      this.options.serviceOptions,
      includeStatus,
    );
    if (includeStatus) this.statusCache.set(runtime.canonicalRoot, before);
    const schema = this.indexSchema(before, input);
    const lease = await this.modelPool.acquire({
      schema,
      root: runtime.canonicalRoot,
      registryHome: before.home,
    });
    let service: Awaited<ReturnType<typeof createZvecGrep>> | undefined;
    try {
      service = await (this.options.createService ?? createZvecGrep)({
        ...this.options.serviceOptions,
        root: runtime.canonicalRoot,
        embeddingModel: lease.model,
        embeddingModelOwnership: "borrowed",
        daemonInstanceToken: this.runtimeManager.instanceToken,
      });
      await service.index({
        root: runtime.canonicalRoot,
        rebuild: input.rebuild,
        resetPaths: input.resetPaths,
        globs: normalizePlainStringList(input.globs),
        insensitiveGlobs: normalizePlainStringList(input.insensitiveGlobs),
        fileTypes: normalizePlainStringList(input.fileTypes),
        excludedFileTypes: normalizePlainStringList(input.excludedFileTypes),
        hidden: input.hidden,
        noIgnore: input.noIgnore,
        ignoreFiles: normalizePlainStringList(input.ignoreFiles),
        maxDepth: input.maxDepth,
        maxFileSizeBytes: input.maxFileSizeBytes,
        follow: input.follow,
        embeddingConcurrency: input.embeddingConcurrency,
        changedPaths: input.changedPaths,
        onProgress: report,
        onWriterContext: (context) => runtime.setWriterContext(context),
      });
    } finally {
      try {
        await service?.close();
      } finally {
        lease.release();
      }
    }

    if (includeStatus) {
      await this.watchers.get(runtime.canonicalRoot)?.flushPending();
    }
    const proofReconciliationEpoch = runtime.reconciliationEpoch();
    const after = await inspectRoot(
      runtime.canonicalRoot,
      this.options.serviceOptions,
      includeStatus,
    );
    if (includeStatus) this.statusCache.set(runtime.canonicalRoot, after);
    if (!after.collection?.embedding) {
      throw new DaemonError(
        "INDEX_MISSING",
        "Index completed without an embedding schema.",
      );
    }
    runtime.updateModelRequest({
      schema: after.collection.embedding,
      root: runtime.canonicalRoot,
      registryHome: after.home,
    });
    this.options.logger?.event("index.completed", {
      root_id: rootIdentity(runtime.canonicalRoot),
      duration_ms: Date.now() - startedAt,
      scope: input.changedPaths ? "paths" : "reconcile",
      changed_paths: input.changedPaths?.length,
    });
    const reconciled = includeStatus && indexStatusIsFresh(after);
    if (includeStatus && !reconciled) {
      runtime.requireFullReconciliation(true);
    }
    return {
      reconciled,
      reconciliationEpoch: proofReconciliationEpoch,
    };
  }

  private async dropIndex(
    input: ZvecGrepIndexInput,
  ): Promise<ZvecGrepIndexResult> {
    assertDropOnlyInput(input);
    const canonicalRoot = await resolveRequestedRoot(input.root, true);
    await this.scheduler.waitForRootIdle(canonicalRoot);
    await this.runtimeManager.evict(canonicalRoot);
    this.statusCache.delete(canonicalRoot);
    const service = await (this.options.createService ?? createZvecGrep)({
      ...this.options.serviceOptions,
      root: canonicalRoot,
      daemonInstanceToken: this.runtimeManager.instanceToken,
    });
    try {
      const dropped = await service.dropIndex({ root: canonicalRoot });
      this.options.logger?.event("index.dropped", {
        root_id: rootIdentity(canonicalRoot),
        dropped,
      });
      return {
        root: canonicalRoot,
        jobId: "drop",
        state: "succeeded",
        reused: false,
        action: "drop",
        dropped,
      };
    } finally {
      await service.close();
    }
  }

  private async submitIndex(
    runtime: RootRuntime,
    input: DaemonIndexInput,
    reason: "background_reconcile" | "fresh_query",
    wait: boolean,
  ): Promise<IndexJobSnapshot> {
    const createsWork =
      !this.scheduler.hasActiveRoot(runtime.canonicalRoot) ||
      input.rebuild === true;
    const targetRevision = createsWork
      ? runtime.markDirty()
      : runtime.snapshot().dirtyRevision;
    const followsNarrowWatch =
      this.scheduler.getByRoot(runtime.canonicalRoot)?.reason === "watch";
    runtime.setWriterPending(true);
    let submitted;
    try {
      submitted = this.scheduler.submit({
        canonicalRoot: runtime.canonicalRoot,
        reason,
        followupIfRunning: followsNarrowWatch,
        run: (report) =>
          runtime.withWrite(async () => {
            const proof = await this.runIndex(runtime, input, report);
            if (proof.reconciled) {
              runtime.markReconciled(targetRevision, proof.reconciliationEpoch);
            } else {
              runtime.markIndexed(targetRevision);
            }
          }),
      });
    } catch (error) {
      runtime.setWriterPending(false);
      throw error;
    }
    if (!submitted.reused) {
      void this.scheduler.waitForRootIdle(runtime.canonicalRoot).finally(() => {
        runtime.setWriterPending(false);
      });
    }
    if (!wait) return submitted.job;
    const job = await this.scheduler.wait(submitted.job.id);
    await this.scheduler.waitForRootIdle(runtime.canonicalRoot);
    runtime.setWriterPending(false);
    return job;
  }

  private ensureWatcher(runtime: RootRuntime): void {
    if (this.watchers.has(runtime.canonicalRoot) || this.shuttingDown) {
      return;
    }
    const coordinator = new IndexCoordinator({
      runtime,
      scheduler: this.scheduler,
      getIndexedFileCount: () =>
        this.statusCache.get(runtime.canonicalRoot)?.status?.filesStored,
      run: async (changes, report) => {
        const changedPaths = [
          ...changes.touchedFiles,
          ...changes.rescanDirectories,
          ...changes.deletedPrefixes,
        ];
        if (!changes.forceFullReconcile && changedPaths.length === 0) {
          return;
        }
        return await this.runIndex(
          runtime,
          {
            root: runtime.canonicalRoot,
            changedPaths: changes.forceFullReconcile ? undefined : changedPaths,
          },
          report,
        );
      },
    });
    const watcher = (
      this.options.watchManagerFactory ??
      ((options) => new WatchManager(options))
    )({
      root: runtime.canonicalRoot,
      onChanges: (changes, reason) => {
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

  private async waitForFresh(
    runtime: RootRuntime,
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
        { root: runtime.canonicalRoot },
        "fresh_query",
        true,
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

  private async probeCurrentFreshness(
    runtime: RootRuntime,
  ): Promise<"fresh" | "stale"> {
    const freshness = await runtime.probeFreshness(async () => {
      const info = await inspectRoot(
        runtime.canonicalRoot,
        this.options.serviceOptions,
      );
      this.statusCache.set(runtime.canonicalRoot, info);
      return indexStatusIsFresh(info);
    });
    this.options.logger?.event(`runtime.recovery_probe_${freshness}`, {
      root_id: rootIdentity(runtime.canonicalRoot),
    });
    return freshness;
  }

  private async closeWatcher(canonicalRoot: string): Promise<void> {
    const watcher = this.watchers.get(canonicalRoot);
    this.watchers.delete(canonicalRoot);
    this.indexCoordinators.delete(canonicalRoot);
    await watcher?.close();
  }

  private indexSchema(
    info: ZvecGrepInfoResult,
    input: ZvecGrepIndexInput,
  ): CollectionEmbeddingSchema {
    if (info.collection?.embedding && !input.embedding) {
      return info.collection.embedding;
    }
    const reference =
      input.embedding ??
      this.options.serviceOptions?.embedding ??
      (this.options.serviceOptions?.defaultEmbedding
        ? DEFAULT_LOCAL_EMBEDDING
        : undefined);
    if (!reference) {
      throw new DaemonError(
        "MODEL_LOAD_FAILED",
        "A new index requires embedding or an explicit server default model.",
      );
    }
    return (
      this.options.resolveEmbeddingSchema ?? resolveCatalogEmbeddingSchema
    )(reference);
  }
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

function resolveCatalogEmbeddingSchema(
  reference: string,
): CollectionEmbeddingSchema {
  const entry = getEmbeddingModelCatalogEntry(reference);
  if (!entry) {
    throw new DaemonError(
      "MODEL_LOAD_FAILED",
      `Server MVP cannot resolve embedding schema for ${reference}.`,
    );
  }
  return {
    provider: entry.provider,
    model: entry.model,
    dimension: entry.dimension,
    metric: entry.metric,
  };
}

function persistentStatus(
  info: ZvecGrepInfoResult,
): ZvecGrepIndexStatusResult["persistent"] {
  return {
    home: info.home,
    index_path: info.indexPath,
    collection: info.collection
      ? {
          id: info.collection.id,
          name: info.collection.name,
          path: info.collection.path,
          root_paths: info.collection.rootPaths.map((rootPath) => ({
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
          embedding: info.collection.embedding,
          index_version: info.collection.indexVersion,
          created_time: info.collection.createdTime,
          updated_time: info.collection.updatedTime,
        }
      : undefined,
    files: info.status
      ? {
          stored: info.status.filesStored,
          indexed: info.status.filesIndexed,
          pending: info.status.filesPending,
          failed: info.status.filesFailed,
          entities: info.status.entitiesIndexed,
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
): NonNullable<ZvecGrepSearchResult["indexing"]> {
  const state = !job || job.state === "succeeded" ? "idle" : job.state;
  const completed = job?.progress?.filesIndexed;
  const total = job?.progress?.filesTotal;
  return {
    state,
    ...(completed === undefined ? {} : { completed }),
    ...(total === undefined ? {} : { total }),
  };
}
