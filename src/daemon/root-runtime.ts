import {
  openWorkspaceReadSession,
  planWorkspaceContext,
  prepareWorkspaceContext,
  type WorkspaceReadSession,
  type WorkspaceContextPlan,
  type WorkspaceContextPreflight,
  type PreparedWorkspaceContext,
} from "../engine/service/zvec-grep.js";
import { EngineError, isEngineError } from "../engine/errors.js";
import { TimingCollector } from "../engine/utils/timing.js";
import { awaitWithSignal } from "../engine/utils/abort.js";
import type {
  ZvecGrepContextOptions,
  ZvecGrepContextResult,
  ZvecGrepWriterContext,
} from "../engine/service/types.js";
import type {
  EmbeddingModelLoadRequest,
  EmbeddingModelPool,
} from "./model-pool.js";
import { WorkspaceReadSessionCache } from "./workspace-read-session-cache.js";
import type { RootLease } from "./root-lease.js";
import { SourceInvalidations } from "./source-invalidations.js";

export type RootRuntimeOptions = {
  canonicalRoot: string;
  modelPool: EmbeddingModelPool;
  modelLoadRequest?: EmbeddingModelLoadRequest;
  rootLease?: RootLease;
  readSessionIdleTtlMs?: number;
  openSession?: () => WorkspaceReadSession | Promise<WorkspaceReadSession>;
  onActivity?: () => void;
};

export type RootSearchExecutionOptions = {
  signal?: AbortSignal;
  /** Budget for query model loading/preparation, not an end-to-end deadline. */
  semanticBudgetMs?: number;
};

type ReadGeneration = {
  cache: WorkspaceReadSessionCache<WorkspaceReadSession>;
};

type WriterReadSession = {
  context: ZvecGrepWriterContext;
  modelKey: string;
  preflight: (plan: WorkspaceContextPlan) => Promise<WorkspaceContextPreflight>;
};

export class RootRuntime {
  readonly canonicalRoot: string;
  readonly sourceInvalidations = new SourceInvalidations();
  private generation?: ReadGeneration;
  private generationTail: Promise<void> = Promise.resolve();
  private modelLoadRequest?: EmbeddingModelLoadRequest;
  private dirtyRevision = 0;
  private indexedRevision = 0;
  private fullReconciliationEpoch = 0;
  private reconciledFullEpoch = -1;
  private nonProbeableFullEpoch = 0;
  private initialFreshnessProbe?: Promise<"fresh" | "stale">;
  private watcherActive = false;
  private watcherPending = false;
  private watcherEpoch = 0;
  private writerPending = false;
  private writerReady?: Promise<void>;
  private writerReadyResolve?: () => void;
  private writer?: WriterReadSession;
  private activeWriterSearches = 0;
  private writerSearchesDrained?: Promise<void>;
  private writerSearchesDrainedResolve?: () => void;
  private activeOperations = 0;
  private readonly searches = new Set<Promise<ZvecGrepContextResult>>();
  private readonly preparations = new Set<Promise<unknown>>();
  private readonly probes = new Set<Promise<"fresh" | "stale">>();
  private readonly shutdown = new AbortController();
  private closed = false;
  private closePromise?: Promise<void>;

  constructor(private readonly options: RootRuntimeOptions) {
    this.canonicalRoot = options.canonicalRoot;
    this.modelLoadRequest = options.modelLoadRequest;
  }

  updateModelLoadRequest(request: EmbeddingModelLoadRequest): void {
    this.modelLoadRequest = request;
  }

  embeddingProvider(): string | undefined {
    return this.modelLoadRequest?.model.provider;
  }

  currentModelLoadRequest(): EmbeddingModelLoadRequest | undefined {
    return this.modelLoadRequest;
  }

  beginActivity(): () => void {
    if (this.closed) {
      throw new Error("Root runtime is closed.");
    }
    this.activeOperations += 1;
    this.options.onActivity?.();
    let released = false;
    return () => {
      if (released) {
        return;
      }
      released = true;
      this.activeOperations = Math.max(0, this.activeOperations - 1);
      this.options.onActivity?.();
    };
  }

  async search(
    options: ZvecGrepContextOptions,
    modelLoadRequest?: EmbeddingModelLoadRequest,
    execution: RootSearchExecutionOptions = {},
  ): Promise<ZvecGrepContextResult> {
    this.options.onActivity?.();
    execution.signal?.throwIfAborted();
    this.assertOpen();
    if (
      execution.semanticBudgetMs !== undefined &&
      (!Number.isFinite(execution.semanticBudgetMs) ||
        execution.semanticBudgetMs <= 0)
    ) {
      throw new Error(
        "Semantic preparation budget must be a positive finite duration.",
      );
    }
    const signal = execution.signal
      ? AbortSignal.any([execution.signal, this.shutdown.signal])
      : this.shutdown.signal;
    const timings = new TimingCollector();
    const search = timings.time("total", () =>
      this.searchPrepared(
        options,
        modelLoadRequest,
        { ...execution, signal },
        timings,
      ),
    );
    this.searches.add(search);
    try {
      const result = await search;
      // Prepared consumption measures only storage work. Preserve it separately
      // and report end-to-end time including model loading and preparation.
      timings.addEntries(
        result.diagnostics.timings?.map((entry) =>
          entry.name === "total" ? { ...entry, name: "storage_total" } : entry,
        ),
      );
      return {
        ...result,
        diagnostics: { ...result.diagnostics, timings: timings.entries() },
      };
    } finally {
      this.searches.delete(search);
    }
  }

  private async searchPrepared(
    options: ZvecGrepContextOptions,
    modelLoadRequest?: EmbeddingModelLoadRequest,
    execution: RootSearchExecutionOptions = {},
    timings = new TimingCollector(),
  ): Promise<ZvecGrepContextResult> {
    const contextOptions = {
      ...options,
      root: this.canonicalRoot,
      autoUpdate: false,
    };
    // Validate and capture all groups before loading a model or sending text.
    const plan = planWorkspaceContext(contextOptions, execution.signal);
    const request = modelLoadRequest ?? this.modelLoadRequest;
    // Explicit vector routes and non-local providers must never be silently
    // converted to local recall, even when an internal caller supplies a budget.
    const requestedBudgetMs =
      request?.model.provider === "local" &&
      plan.groups.some((group) => group.role === "primary") &&
      !plan.options.routes?.some((route) => route.mode === "vector")
        ? execution.semanticBudgetMs
        : undefined;
    for (let attempt = 0; attempt < 2; attempt++) {
      const preflight = await this.withWorkspaceReader(
        (session) => session.preflight(plan),
        (writer) => writer.preflight(plan),
        undefined,
        execution.signal,
      );
      execution.signal?.throwIfAborted();
      this.assertOpen();
      if (preflight.requiresEmbedding && !request) {
        throw new Error("Root runtime does not have an embedding model.");
      }
      // A rebuild may have changed providers since the backend resolved the
      // model request. Only the actual indexed provider can opt into fallback.
      const budgetMs =
        preflight.workspaceIndex.embedding?.provider === "local"
          ? requestedBudgetMs
          : undefined;
      // No storage handle, generation lock or writer read is held while a
      // model loads or embeds. FTS and index writes can proceed independently.
      const expired = new Error("Semantic preparation budget exceeded.");
      const deadline = new AbortController();
      const preparationSignal = execution.signal
        ? AbortSignal.any([execution.signal, deadline.signal])
        : deadline.signal;
      const timer =
        preflight.requiresEmbedding && budgetMs !== undefined
          ? setTimeout(() => deadline.abort(expired), budgetMs)
          : undefined;
      let preparation: {
        prepared: PreparedWorkspaceContext;
        modelKey?: string;
      };
      try {
        preparation = await timings.time("semantic_preparation", () =>
          awaitWithSignal(
            this.prepare(plan, preflight, request, preparationSignal),
            preparationSignal,
          ),
        );
      } catch (error) {
        execution.signal?.throwIfAborted();
        if (error !== expired || budgetMs === undefined) throw error;
        return this.searchLocalOnly(plan, budgetMs, execution.signal);
      } finally {
        clearTimeout(timer);
      }
      execution.signal?.throwIfAborted();
      this.assertOpen();
      const { prepared, modelKey } = preparation;
      try {
        return await this.withWorkspaceReader(
          async (session) =>
            this.recordSourceInvalidations(
              await session.contextPrepared(prepared),
            ),
          async (writer) =>
            this.recordSourceInvalidations(
              await writer.context(prepared.plan.options, prepared),
            ),
          modelKey,
          execution.signal,
        );
      } catch (error) {
        if (
          attempt !== 0 ||
          !isEngineError(error) ||
          error.code !== "ZVEC_GREP.ENGINE.SEARCH.PREPARED_VECTORS_REQUIRED"
        ) {
          throw error;
        }
        // A formerly empty filter gained files while we prepared. Recheck
        // once; never silently omit a vector route or retry a schema mismatch.
      }
    }
    throw new Error("Search preparation did not settle.");
  }

  private prepare(
    plan: WorkspaceContextPlan,
    preflight: WorkspaceContextPreflight,
    request: EmbeddingModelLoadRequest | undefined,
    signal: AbortSignal,
  ): Promise<{ prepared: PreparedWorkspaceContext; modelKey?: string }> {
    const preparation = (async () => {
      signal.throwIfAborted();
      const lease = preflight.requiresEmbedding
        ? await this.options.modelPool.acquire(request!)
        : undefined;
      try {
        // A shared model load may finish after this request has returned. Never
        // start a late query; only release its lease once ownership arrives.
        signal.throwIfAborted();
        this.assertOpen();
        const prepared = await prepareWorkspaceContext(
          plan,
          lease?.model,
          preflight,
          signal,
        );
        return { prepared, modelKey: lease?.key };
      } finally {
        lease?.release();
      }
    })();
    this.preparations.add(preparation);
    void preparation.then(
      () => {
        this.preparations.delete(preparation);
      },
      () => {
        this.preparations.delete(preparation);
      },
    );
    return preparation;
  }

  private async searchLocalOnly(
    requested: WorkspaceContextPlan,
    budgetMs: number,
    signal?: AbortSignal,
  ): Promise<ZvecGrepContextResult> {
    const plan = planWorkspaceContext(requested.options, signal, "fts_only");
    const consume = async (
      preflight: WorkspaceContextPreflight,
      read: (
        prepared: PreparedWorkspaceContext,
      ) => Promise<ZvecGrepContextResult>,
    ) => {
      // Validate and consume under one short read: a provider change during
      // preparation must not turn a remote index into a budgeted local search.
      if (preflight.workspaceIndex.embedding?.provider !== "local") {
        throw new EngineError(
          "Workspace embedding provider changed during search preparation; retry the query",
          { code: "ZVEC_GREP.ENGINE.SEARCH.LOCAL_FALLBACK_PROVIDER_CHANGED" },
        );
      }
      const prepared = await prepareWorkspaceContext(
        plan,
        undefined,
        preflight,
        signal,
      );
      signal?.throwIfAborted();
      return this.recordSourceInvalidations(await read(prepared));
    };
    const result = await this.withWorkspaceReader(
      async (session) =>
        consume(await session.preflight(plan), (prepared) =>
          session.contextPrepared(prepared),
        ),
      async (writer) =>
        consume(await writer.preflight(plan), (prepared) =>
          writer.context(prepared.plan.options, prepared),
        ),
      undefined,
      signal,
    );
    return {
      ...result,
      diagnostics: {
        ...result.diagnostics,
        emptyReason:
          result.items.length === 0
            ? "semantic_incomplete"
            : result.diagnostics.emptyReason,
        semantic: {
          status: "skipped",
          reason: "preparation_budget_exceeded",
          budgetMs,
        },
      },
    };
  }

  private async withWorkspaceReader<T>(
    read: (session: WorkspaceReadSession) => Promise<T>,
    readWriter: (writer: WriterReadSession) => Promise<T>,
    modelKey?: string,
    signal?: AbortSignal,
  ): Promise<T> {
    while (true) {
      signal?.throwIfAborted();
      this.assertOpen();
      const writer = this.writer;
      if (writer && (modelKey === undefined || modelKey === writer.modelKey)) {
        return await awaitWithSignal(
          this.withWriterRead(() => readWriter(writer)),
          signal,
        );
      }
      if (!this.writerPending || !this.writerReady) {
        break;
      }
      await awaitWithSignal(this.writerReady, signal);
    }

    return awaitWithSignal(
      this.runGenerationSerial(async () => {
        signal?.throwIfAborted();
        this.assertOpen();
        if (!this.generation) {
          this.generation = {
            cache: new WorkspaceReadSessionCache({
              open: () =>
                this.options.openSession
                  ? this.options.openSession()
                  : openWorkspaceReadSession(this.canonicalRoot),
              idleTtlMs: this.options.readSessionIdleTtlMs,
              serializeOperations: true,
            }),
          };
        }

        return this.generation.cache.withRead(read);
      }),
      signal,
    );
  }

  private recordSourceInvalidations(
    result: ZvecGrepContextResult,
  ): ZvecGrepContextResult {
    // Record inside the actual storage callback, before a writer can replace
    // this generation and acknowledge its repair. A canceled waiter may have
    // detached already, but its real read still owns this evidence.
    this.sourceInvalidations.record(
      result.diagnostics.index?.sourceInvalidations ?? [],
    );
    return result;
  }

  setWriterPending(pending: boolean): void {
    if (pending === this.writerPending) {
      return;
    }
    this.writerPending = pending;
    if (pending) {
      this.armWriterReady();
    } else {
      this.notifyWriterStateChanged();
    }
  }

  setWriterContext(
    context: ZvecGrepWriterContext,
    modelKey: string,
    preflight: WriterReadSession["preflight"],
  ): () => Promise<void> {
    const writer = { context, modelKey, preflight };
    this.writer = writer;
    this.notifyWriterStateChanged();
    this.armWriterReady();
    return async () => {
      if (this.writer !== writer) {
        return;
      }
      this.writer = undefined;
      this.notifyWriterStateChanged();
      this.armWriterReady();
      await this.drainWriterReads();
    };
  }

  markDirty(): number {
    this.dirtyRevision += 1;
    return this.dirtyRevision;
  }

  markIndexed(revision = this.dirtyRevision): void {
    this.indexedRevision = Math.max(this.indexedRevision, revision);
  }

  requireFullReconciliation(probeAllowed = false): void {
    this.fullReconciliationEpoch += 1;
    if (!probeAllowed) {
      this.nonProbeableFullEpoch = this.fullReconciliationEpoch;
    }
  }

  private armWriterReady(): void {
    if (!this.writerPending || this.writerReady) {
      return;
    }
    this.writerReady = new Promise<void>((resolve) => {
      this.writerReadyResolve = resolve;
    });
  }

  private notifyWriterStateChanged(): void {
    const resolve = this.writerReadyResolve;
    this.writerReadyResolve = undefined;
    this.writerReady = undefined;
    resolve?.();
  }

  reconciliationEpoch(): number {
    return this.fullReconciliationEpoch;
  }

  markReconciled(
    revision = this.dirtyRevision,
    reconciliationEpoch = this.fullReconciliationEpoch,
  ): void {
    this.markIndexed(revision);
    this.reconciledFullEpoch = Math.max(
      this.reconciledFullEpoch,
      reconciliationEpoch,
    );
  }

  requiresFullReconciliation(): boolean {
    return this.reconciledFullEpoch < this.fullReconciliationEpoch;
  }

  canProbeFullReconciliation(): boolean {
    return this.nonProbeableFullEpoch <= this.reconciledFullEpoch;
  }

  needsReconciliation(): boolean {
    return (
      this.sourceInvalidations.hasPending() ||
      this.requiresFullReconciliation() ||
      this.indexedRevision < this.dirtyRevision
    );
  }

  hasKnownChanges(): boolean {
    return (
      this.sourceInvalidations.hasPending() ||
      this.indexedRevision < this.dirtyRevision ||
      (this.requiresFullReconciliation() && !this.canProbeFullReconciliation())
    );
  }

  probeInitialFreshness(
    probe: () => Promise<boolean>,
    onResult?: (result: "fresh" | "stale") => void,
  ): Promise<"fresh" | "stale"> {
    this.initialFreshnessProbe ??= this.probeFreshness(probe).then((result) => {
      onResult?.(result);
      return result;
    });
    return this.initialFreshnessProbe;
  }

  probeFreshness(probe: () => Promise<boolean>): Promise<"fresh" | "stale"> {
    const release = this.beginActivity();
    const operation = this.runFreshnessProbe(probe);
    this.probes.add(operation);
    const finish = () => {
      this.probes.delete(operation);
      release();
    };
    void operation.then(finish, finish);
    return operation;
  }

  setWatcherActive(active: boolean): void {
    this.watcherActive = active;
  }

  recordWatcherActivity(): void {
    if (!this.closed) this.options.onActivity?.();
  }

  setWatcherPending(pending: boolean): void {
    if (pending) {
      this.watcherEpoch += 1;
    }
    this.watcherPending = pending;
  }

  async withWrite<T>(operation: () => Promise<T>): Promise<T> {
    this.assertOpen();
    this.setWriterPending(true);
    try {
      return await this.runGenerationSerial(async () => {
        const generation = this.generation;
        this.generation = undefined;
        await generation?.cache.close();
        return operation();
      });
    } finally {
      this.setWriterPending(false);
    }
  }

  snapshot(): {
    readSessionOpen: boolean;
    activeReaders: number;
    activeOperations: number;
    writerPending: boolean;
    dirtyRevision: number;
    indexedRevision: number;
    watcherActive: boolean;
    watcherPending: boolean;
    watcherEpoch: number;
  } {
    const read = this.generation?.cache.snapshot();
    return {
      readSessionOpen: read?.open ?? false,
      activeReaders: read?.activeReaders ?? 0,
      activeOperations: this.activeOperations,
      writerPending: this.writerPending,
      dirtyRevision: this.dirtyRevision,
      indexedRevision: this.indexedRevision,
      watcherActive: this.watcherActive,
      watcherPending: this.watcherPending,
      watcherEpoch: this.watcherEpoch,
    };
  }

  async close(): Promise<void> {
    if (this.closePromise) return this.closePromise;
    this.closed = true;
    // Install the single-flight promise before synchronously notifying model
    // abort listeners: a listener may itself request shutdown.
    this.closePromise = Promise.resolve().then(async () => {
      // Drain model preparation outside generationTail: prepared requests must
      // be able to observe closed and release their short model leases.
      await Promise.allSettled([...this.searches]);
      await Promise.allSettled([...this.preparations]);
      await Promise.allSettled([...this.probes]);
      // Aborted foreground waiters can finish before their actual writer read.
      // Keep root ownership until those reads have run their finally cleanup.
      await this.drainWriterReads();
      await this.runGenerationSerial(async () => {
        const generation = this.generation;
        this.generation = undefined;
        try {
          await generation?.cache.close();
        } finally {
          await this.options.rootLease?.release();
        }
      });
    });
    this.shutdown.abort(new Error("Root runtime is closed."));
    this.watcherActive = false;
    this.watcherPending = false;
    this.setWriterPending(false);
    return this.closePromise;
  }

  private async withWriterRead<T>(operation: () => Promise<T>): Promise<T> {
    this.activeWriterSearches += 1;
    try {
      return await operation();
    } finally {
      this.activeWriterSearches -= 1;
      if (this.activeWriterSearches === 0) {
        this.writerSearchesDrainedResolve?.();
        this.writerSearchesDrainedResolve = undefined;
        this.writerSearchesDrained = undefined;
      }
    }
  }

  private async drainWriterReads(): Promise<void> {
    if (this.activeWriterSearches === 0) return;
    this.writerSearchesDrained ??= new Promise<void>((resolve) => {
      this.writerSearchesDrainedResolve = resolve;
    });
    await this.writerSearchesDrained;
  }

  private assertOpen(): void {
    if (this.closed) throw new Error("Root runtime is closed.");
  }

  private async runFreshnessProbe(
    probe: () => Promise<boolean>,
  ): Promise<"fresh" | "stale"> {
    const revision = this.dirtyRevision;
    const watcherEpoch = this.watcherEpoch;
    const reconciliationEpoch = this.fullReconciliationEpoch;
    let fresh = false;
    try {
      fresh = await probe();
    } catch {
      return "stale";
    }
    if (
      this.closed ||
      !fresh ||
      this.dirtyRevision !== revision ||
      this.watcherEpoch !== watcherEpoch ||
      this.fullReconciliationEpoch !== reconciliationEpoch
    ) {
      return "stale";
    }
    this.markReconciled(revision, reconciliationEpoch);
    // Metadata can reconcile watcher uncertainty, never contrary content-hash
    // evidence. Only a per-path proof from an index operation can clear that.
    return this.sourceInvalidations.hasPending() ? "stale" : "fresh";
  }

  private async runGenerationSerial<T>(
    operation: () => Promise<T>,
  ): Promise<T> {
    const previous = this.generationTail;
    let release!: () => void;
    this.generationTail = new Promise<void>((resolve) => {
      release = resolve;
    });
    await previous;
    try {
      return await operation();
    } finally {
      release();
    }
  }
}
