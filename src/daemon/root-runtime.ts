import type { AnonymousReadSession } from "../engine/service/zvec-grep.js";
import { openAnonymousReadSession } from "../engine/service/zvec-grep.js";
import type {
  ZvecGrepContextOptions,
  ZvecGrepContextResult,
} from "../engine/service/types.js";
import type {
  EmbeddingModelPool,
  ModelLease,
  ModelLeaseRequest,
} from "./model-pool.js";
import { ReadCollectionCache } from "./read-collection-cache.js";
import type { RootLease } from "./root-lease.js";

export type RootRuntimeOptions = {
  canonicalRoot: string;
  modelPool: EmbeddingModelPool;
  modelRequest?: ModelLeaseRequest;
  rootLease?: RootLease;
  readCollectionIdleTtlMs?: number;
  openSession?: (
    lease: ModelLease,
  ) => AnonymousReadSession | Promise<AnonymousReadSession>;
  onActivity?: () => void;
};

type LeasedReadSession = AnonymousReadSession & {
  readonly modelKey: string;
};

type ReadGeneration = {
  key: string;
  cache: ReadCollectionCache<LeasedReadSession>;
};

export class RootRuntime {
  readonly canonicalRoot: string;
  private generation?: ReadGeneration;
  private generationTail: Promise<void> = Promise.resolve();
  private modelRequest?: ModelLeaseRequest;
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
  private activeWriterSearches = 0;
  private writerSearchesDrained?: Promise<void>;
  private writerSearchesDrainedResolve?: () => void;
  private closed = false;

  constructor(private readonly options: RootRuntimeOptions) {
    this.canonicalRoot = options.canonicalRoot;
    this.modelRequest = options.modelRequest;
  }

  updateModelRequest(request: ModelLeaseRequest): void {
    this.modelRequest = request;
  }

  embeddingProvider(): string | undefined {
    return this.modelRequest?.schema.provider;
  }

  async search(
    options: ZvecGrepContextOptions,
  ): Promise<ZvecGrepContextResult> {
    this.options.onActivity?.();
    if (this.closed) {
      throw new Error("Root runtime is closed.");
    }
    const writerContext = this.writerContext;
    if (writerContext) {
      return await this.withWriterContext(writerContext, options);
    }
    while (this.writerPending && this.writerReady) {
      await this.writerReady;
    }
    return this.runGenerationSerial(async () => {
      if (this.closed) {
        throw new Error("Root runtime is closed.");
      }
      const request = this.modelRequest;
      if (!request) {
        throw new Error(
          "Root runtime does not have an indexed embedding schema.",
        );
      }
      const desiredKey = this.options.modelPool.keyFor(request);
      if (this.generation?.key !== desiredKey) {
        await this.generation?.cache.close();
        this.generation = {
          key: desiredKey,
          cache: new ReadCollectionCache({
            open: () => this.openLeasedSession(request),
            idleTtlMs: this.options.readCollectionIdleTtlMs,
            serializeOperations: true,
          }),
        };
      }

      return this.generation.cache.withRead((session) =>
        session.context({
          ...options,
          root: this.canonicalRoot,
          autoUpdate: false,
        }),
      );
    });
  }

  setWriterPending(pending: boolean): void {
    if (pending === this.writerPending) {
      return;
    }
    this.writerPending = pending;
    if (pending) {
      this.writerReady = new Promise<void>((resolve) => {
        this.writerReadyResolve = resolve;
      });
    } else {
      this.writerReadyResolve?.();
      this.writerReadyResolve = undefined;
      this.writerReady = undefined;
    }
  }

  setWriterContext(
    context: (
      options: ZvecGrepContextOptions,
    ) => Promise<ZvecGrepContextResult>,
  ): () => Promise<void> {
    this.writerContext = context;
    return async () => {
      if (this.writerContext !== context) {
        return;
      }
      this.writerContext = undefined;
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
    this.options.onActivity?.();
    try {
      return await this.runGenerationSerial(async () => {
        const generation = this.generation;
        this.generation = undefined;
        await generation?.cache.close();
        return operation();
      });
    } finally {
      this.options.onActivity?.();
    }
  }

  snapshot(): {
    readCollectionOpen: boolean;
    activeReaders: number;
    writerPending: boolean;
    dirtyRevision: number;
    indexedRevision: number;
    watcherActive: boolean;
    watcherPending: boolean;
    watcherEpoch: number;
  } {
    const read = this.generation?.cache.snapshot();
    return {
      readCollectionOpen: read?.open ?? false,
      activeReaders: read?.activeReaders ?? 0,
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
      this.generation = undefined;
      try {
        await generation?.cache.close();
      } finally {
        await this.options.rootLease?.release();
      }
    });
  }

  private async openLeasedSession(
    request: ModelLeaseRequest,
  ): Promise<LeasedReadSession> {
    const lease = await this.options.modelPool.acquire(request);
    let session: AnonymousReadSession;
    try {
      session = this.options.openSession
        ? await this.options.openSession(lease)
        : openAnonymousReadSession(this.options.canonicalRoot, lease.model);
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
