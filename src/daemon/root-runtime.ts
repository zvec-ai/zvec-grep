import type {
  WorkspaceGraphReadSession,
  WorkspaceReadSession,
} from "../engine/service/zvec-grep.js";
import {
  openWorkspaceGraphReadSession,
  openWorkspaceReadSession,
} from "../engine/service/zvec-grep.js";
import type {
  ZvecGrepContextOptions,
  ZvecGrepContextResult,
  ZvecGrepExploreOptions,
  ZvecGrepExploreResult,
  ZvecGrepGraphNeighborhoodOptions,
  ZvecGrepGraphNeighborhoodResult,
} from "../engine/service/types.js";
import type {
  EmbeddingModelLoadRequest,
  EmbeddingModelPool,
  ModelLease,
} from "./model-pool.js";
import { WorkspaceReadSessionCache } from "./workspace-read-session-cache.js";
import type { RootLease } from "./root-lease.js";

export type RootRuntimeOptions = {
  canonicalRoot: string;
  modelPool: EmbeddingModelPool;
  modelLoadRequest?: EmbeddingModelLoadRequest;
  rootLease?: RootLease;
  readSessionIdleTtlMs?: number;
  openSession?: (
    lease: ModelLease,
  ) => WorkspaceReadSession | Promise<WorkspaceReadSession>;
  openGraphSession?: () =>
    WorkspaceGraphReadSession | Promise<WorkspaceGraphReadSession>;
  onActivity?: () => void;
};

type LeasedReadSession = WorkspaceReadSession & {
  readonly modelKey: string;
};

type ReadGeneration = {
  key: string;
  cache: WorkspaceReadSessionCache<LeasedReadSession>;
};

export class RootRuntime {
  readonly canonicalRoot: string;
  private generation?: ReadGeneration;
  private graphCache?: WorkspaceReadSessionCache<WorkspaceGraphReadSession>;
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
  private writerContext?: (
    options: ZvecGrepContextOptions,
  ) => Promise<ZvecGrepContextResult>;
  private writerModelKey?: string;
  private activeWriterSearches = 0;
  private writerSearchesDrained?: Promise<void>;
  private writerSearchesDrainedResolve?: () => void;
  private activeOperations = 0;
  private closed = false;

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
  ): Promise<ZvecGrepContextResult> {
    this.options.onActivity?.();
    if (this.closed) {
      throw new Error("Root runtime is closed.");
    }
    const overrideModelKey = modelLoadRequest
      ? this.options.modelPool.keyFor(modelLoadRequest)
      : undefined;
    while (true) {
      const writerContext = this.writerContext;
      if (
        writerContext &&
        (overrideModelKey === undefined ||
          overrideModelKey === this.writerModelKey)
      ) {
        return await this.withWriterContext(writerContext, options);
      }
      if (!this.writerPending || !this.writerReady) {
        break;
      }
      await this.writerReady;
    }

    return this.withReadSession(modelLoadRequest, (session) =>
      session.context({
        ...options,
        root: this.canonicalRoot,
        autoUpdate: false,
      }),
    );
  }

  async explore(
    options: ZvecGrepExploreOptions,
  ): Promise<ZvecGrepExploreResult> {
    await this.waitForWriter();
    return this.withGraphReadSession((session) =>
      session.explore({ ...options, root: this.canonicalRoot }),
    );
  }

  async graphNeighborhood(
    options: ZvecGrepGraphNeighborhoodOptions,
  ): Promise<ZvecGrepGraphNeighborhoodResult> {
    await this.waitForWriter();
    return this.withGraphReadSession((session) =>
      session.graphNeighborhood({ ...options, root: this.canonicalRoot }),
    );
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
    context: (
      options: ZvecGrepContextOptions,
    ) => Promise<ZvecGrepContextResult>,
    modelKey: string,
  ): () => Promise<void> {
    this.writerContext = context;
    this.writerModelKey = modelKey;
    this.notifyWriterStateChanged();
    this.armWriterReady();
    return async () => {
      if (this.writerContext !== context) {
        return;
      }
      this.writerContext = undefined;
      this.writerModelKey = undefined;
      this.notifyWriterStateChanged();
      this.armWriterReady();
      if (this.activeWriterSearches > 0) {
        this.writerSearchesDrained ??= new Promise<void>((resolve) => {
          this.writerSearchesDrainedResolve = resolve;
        });
        await this.writerSearchesDrained;
      }
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
      this.requiresFullReconciliation() ||
      this.indexedRevision < this.dirtyRevision
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
    return this.runFreshnessProbe(probe);
  }

  setWatcherActive(active: boolean): void {
    this.watcherActive = active;
  }

  setWatcherPending(pending: boolean): void {
    if (pending) {
      this.watcherEpoch += 1;
    }
    this.watcherPending = pending;
    this.options.onActivity?.();
  }

  async withWrite<T>(operation: () => Promise<T>): Promise<T> {
    this.setWriterPending(true);
    try {
      return await this.runGenerationSerial(async () => {
        const generation = this.generation;
        const graphCache = this.graphCache;
        this.generation = undefined;
        this.graphCache = undefined;
        await Promise.all([generation?.cache.close(), graphCache?.close()]);
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
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.watcherActive = false;
    this.watcherPending = false;
    this.setWriterPending(false);
    await this.runGenerationSerial(async () => {
      const generation = this.generation;
      const graphCache = this.graphCache;
      this.generation = undefined;
      this.graphCache = undefined;
      try {
        await Promise.all([generation?.cache.close(), graphCache?.close()]);
      } finally {
        await this.options.rootLease?.release();
      }
    });
  }

  private async openLeasedSession(
    request: EmbeddingModelLoadRequest,
  ): Promise<LeasedReadSession> {
    const lease = await this.options.modelPool.acquire(request);
    let session: WorkspaceReadSession;
    try {
      session = this.options.openSession
        ? await this.options.openSession(lease)
        : openWorkspaceReadSession(this.options.canonicalRoot, lease.model);
    } catch (error) {
      lease.release();
      throw error;
    }
    let closed = false;
    return {
      root: session.root,
      modelKey: lease.key,
      context: (contextOptions) => session.context(contextOptions),
      async close() {
        if (closed) {
          return;
        }
        closed = true;
        try {
          await session.close();
        } finally {
          lease.release();
        }
      },
    };
  }

  private async waitForWriter(): Promise<void> {
    while (this.writerPending && this.writerReady) {
      await this.writerReady;
    }
  }

  private async withGraphReadSession<T>(
    fn: (session: WorkspaceGraphReadSession) => Promise<T>,
  ): Promise<T> {
    return this.runGenerationSerial(async () => {
      if (this.closed) throw new Error("Root runtime is closed.");
      this.graphCache ??= new WorkspaceReadSessionCache({
        open: () =>
          this.options.openGraphSession?.() ??
          openWorkspaceGraphReadSession(this.canonicalRoot),
        idleTtlMs: this.options.readSessionIdleTtlMs,
        serializeOperations: true,
      });
      return this.graphCache.withRead(fn);
    });
  }

  private async withReadSession<T>(
    modelLoadRequest: EmbeddingModelLoadRequest | undefined,
    fn: (session: LeasedReadSession) => Promise<T>,
  ): Promise<T> {
    return this.runGenerationSerial(async () => {
      if (this.closed) throw new Error("Root runtime is closed.");
      const request = modelLoadRequest ?? this.modelLoadRequest;
      if (!request)
        throw new Error("Root runtime does not have an embedding model.");
      const desiredKey = this.options.modelPool.keyFor(request);
      if (this.generation?.key !== desiredKey) {
        await this.generation?.cache.close();
        this.generation = {
          key: desiredKey,
          cache: new WorkspaceReadSessionCache({
            open: () => this.openLeasedSession(request),
            idleTtlMs: this.options.readSessionIdleTtlMs,
            serializeOperations: true,
          }),
        };
      }
      return this.generation.cache.withRead(fn);
    });
  }

  private async withWriterContext(
    context: (
      options: ZvecGrepContextOptions,
    ) => Promise<ZvecGrepContextResult>,
    options: ZvecGrepContextOptions,
  ): Promise<ZvecGrepContextResult> {
    this.activeWriterSearches += 1;
    try {
      return await context({
        ...options,
        root: this.canonicalRoot,
        autoUpdate: false,
      });
    } finally {
      this.activeWriterSearches -= 1;
      if (this.activeWriterSearches === 0) {
        this.writerSearchesDrainedResolve?.();
        this.writerSearchesDrainedResolve = undefined;
        this.writerSearchesDrained = undefined;
      }
    }
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
      !fresh ||
      this.dirtyRevision !== revision ||
      this.watcherEpoch !== watcherEpoch ||
      this.fullReconciliationEpoch !== reconciliationEpoch
    ) {
      return "stale";
    }
    this.markReconciled(revision, reconciliationEpoch);
    return "fresh";
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
