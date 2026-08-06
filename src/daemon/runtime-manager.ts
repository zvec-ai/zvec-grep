import { access, realpath, stat } from "node:fs/promises";
import { constants as fsConstants } from "node:fs";
import { isAbsolute } from "node:path";
import {
  createZvecGrep,
  type ZvecGrepInfoResult,
} from "../engine/service/index.js";
import type { CreateZvecGrepOptions } from "../engine/service/types.js";
import { DaemonError } from "./errors.js";
import type {
  EmbeddingModelLoadRequest,
  EmbeddingModelPool,
} from "./model-pool.js";
import { RootRuntime } from "./root-runtime.js";
import { RootLeaseManager } from "./root-lease.js";

export type RuntimeManagerOptions = {
  modelPool: EmbeddingModelPool;
  serviceOptions?: CreateZvecGrepOptions;
  readSessionIdleTtlMs?: number;
  runtimeIdleTtlMs?: number;
  onRuntimeEvicted?: (canonicalRoot: string) => void | Promise<void>;
  rootLeaseManager?: RootLeaseManager;
  createRuntime?: (input: {
    canonicalRoot: string;
    modelLoadRequest?: EmbeddingModelLoadRequest;
    modelPool: EmbeddingModelPool;
  }) => RootRuntime | Promise<RootRuntime>;
};

export type RuntimeManagerSnapshot = {
  activeRuntimes: number;
};

export class RuntimeManager {
  private readonly runtimes = new Map<string, RootRuntime>();
  private readonly creating = new Map<string, Promise<RootRuntime>>();
  private readonly aliases = new Map<string, string>();
  private readonly rootLeaseManager: RootLeaseManager;
  private readonly idleTimers = new Map<
    string,
    ReturnType<typeof setTimeout>
  >();
  private closed = false;

  constructor(private readonly options: RuntimeManagerOptions) {
    this.rootLeaseManager = options.rootLeaseManager ?? new RootLeaseManager();
  }

  get instanceToken(): string {
    return this.rootLeaseManager.instanceToken;
  }

  async activate(requestedRoot: string): Promise<RootRuntime> {
    if (this.closed) {
      throw new DaemonError(
        "DAEMON_SHUTTING_DOWN",
        "The daemon is shutting down.",
        true,
      );
    }
    const canonicalRequestedRoot = await resolveRequestedRoot(
      requestedRoot,
      false,
    );
    const activeRequestedRoot = this.runtimes.get(
      this.aliases.get(canonicalRequestedRoot) ?? canonicalRequestedRoot,
    );
    if (
      activeRequestedRoot &&
      (activeRequestedRoot.snapshot().writerPending ||
        activeRequestedRoot.needsReconciliation())
    ) {
      return activeRequestedRoot;
    }
    const creatingRequestedRoot = this.creating.get(
      this.aliases.get(canonicalRequestedRoot) ?? canonicalRequestedRoot,
    );
    if (creatingRequestedRoot) {
      return creatingRequestedRoot;
    }
    const info = await inspectRoot(
      requestedRoot,
      this.options.serviceOptions,
      false,
    );
    if (!info.indexed || !info.workspaceIndex?.embedding) {
      throw new DaemonError(
        "INDEX_MISSING",
        `Indexed search requires a built zvec-grep index for ${info.root}. Use native grep or rg for exhaustive literal or regex search, or call zvec_grep_index only when persistent indexing is authorized.`,
      );
    }
    const canonicalRoot = await realpath(info.root);
    this.aliases.set(canonicalRequestedRoot, canonicalRoot);
    return this.getOrCreate(canonicalRoot, {
      model: {
        provider: info.workspaceIndex.embedding.provider,
        name: info.workspaceIndex.embedding.model,
      },
    });
  }

  async activateForIndex(requestedRoot: string): Promise<RootRuntime> {
    if (this.closed) {
      throw new DaemonError(
        "DAEMON_SHUTTING_DOWN",
        "The daemon is shutting down.",
        true,
      );
    }
    const canonicalRequestedRoot = await resolveRequestedRoot(
      requestedRoot,
      true,
    );
    const activeRequestedRoot = this.runtimes.get(
      this.aliases.get(canonicalRequestedRoot) ?? canonicalRequestedRoot,
    );
    if (activeRequestedRoot) {
      return activeRequestedRoot;
    }
    const creatingRequestedRoot = this.creating.get(
      this.aliases.get(canonicalRequestedRoot) ?? canonicalRequestedRoot,
    );
    if (creatingRequestedRoot) {
      return creatingRequestedRoot;
    }
    const info = await inspectRoot(
      canonicalRequestedRoot,
      this.options.serviceOptions,
      false,
    );
    const canonicalRoot = await resolveRequestedRoot(info.root, true);
    this.aliases.set(canonicalRequestedRoot, canonicalRoot);
    return this.getOrCreate(canonicalRoot);
  }

  async peek(requestedRoot: string): Promise<RootRuntime | undefined> {
    const info = await inspectRoot(
      requestedRoot,
      this.options.serviceOptions,
      false,
    );
    const canonicalRoot = await realpath(info.root);
    return this.runtimes.get(canonicalRoot);
  }

  getByCanonicalRoot(canonicalRoot: string): RootRuntime | undefined {
    return this.runtimes.get(canonicalRoot);
  }

  snapshot(): RuntimeManagerSnapshot {
    return { activeRuntimes: this.runtimes.size };
  }

  async evict(canonicalRoot: string): Promise<boolean> {
    const creating = this.creating.get(canonicalRoot);
    if (creating) {
      await creating;
    }
    const timer = this.idleTimers.get(canonicalRoot);
    if (timer) clearTimeout(timer);
    this.idleTimers.delete(canonicalRoot);
    const runtime = this.runtimes.get(canonicalRoot);
    this.runtimes.delete(canonicalRoot);
    for (const [alias, target] of this.aliases) {
      if (alias === canonicalRoot || target === canonicalRoot) {
        this.aliases.delete(alias);
      }
    }
    if (!runtime) return false;
    try {
      await this.options.onRuntimeEvicted?.(canonicalRoot);
    } finally {
      await runtime.close();
    }
    return true;
  }

  async close(): Promise<void> {
    if (this.closed) {
      return;
    }
    this.closed = true;
    await Promise.allSettled(this.creating.values());
    const runtimes = [...this.runtimes.values()];
    for (const timer of this.idleTimers.values()) clearTimeout(timer);
    this.idleTimers.clear();
    this.runtimes.clear();
    this.aliases.clear();
    await Promise.all(runtimes.map((runtime) => runtime.close()));
    await this.rootLeaseManager.close();
  }

  private async getOrCreate(
    canonicalRoot: string,
    modelLoadRequest?: EmbeddingModelLoadRequest,
  ): Promise<RootRuntime> {
    const existing = this.runtimes.get(canonicalRoot);
    if (existing) {
      if (modelLoadRequest) {
        existing.updateModelLoadRequest(modelLoadRequest);
      }
      this.touchRuntime(canonicalRoot);
      return existing;
    }
    let pending = this.creating.get(canonicalRoot);
    if (!pending) {
      pending = this.createRuntime(canonicalRoot, modelLoadRequest);
      this.creating.set(canonicalRoot, pending);
    }
    try {
      const runtime = await pending;
      if (modelLoadRequest) {
        runtime.updateModelLoadRequest(modelLoadRequest);
      }
      this.touchRuntime(canonicalRoot);
      return runtime;
    } finally {
      this.creating.delete(canonicalRoot);
    }
  }

  private async createRuntime(
    canonicalRoot: string,
    modelLoadRequest?: EmbeddingModelLoadRequest,
  ): Promise<RootRuntime> {
    let runtime: RootRuntime;
    if (this.options.createRuntime) {
      runtime = await this.options.createRuntime({
        canonicalRoot,
        modelLoadRequest,
        modelPool: this.options.modelPool,
      });
    } else {
      const rootLease = await this.rootLeaseManager.acquire(canonicalRoot);
      runtime = new RootRuntime({
        canonicalRoot,
        modelPool: this.options.modelPool,
        modelLoadRequest,
        rootLease,
        readSessionIdleTtlMs: this.options.readSessionIdleTtlMs,
        onActivity: () => this.touchRuntime(canonicalRoot),
      });
    }
    if (this.closed) {
      await runtime.close();
      throw new DaemonError(
        "DAEMON_SHUTTING_DOWN",
        "The daemon is shutting down.",
        true,
      );
    }
    this.runtimes.set(canonicalRoot, runtime);
    this.touchRuntime(canonicalRoot);
    return runtime;
  }

  private touchRuntime(canonicalRoot: string): void {
    const idleTtlMs = this.options.runtimeIdleTtlMs ?? 30 * 60_000;
    const existing = this.idleTimers.get(canonicalRoot);
    if (existing) clearTimeout(existing);
    if (idleTtlMs <= 0 || this.closed) {
      return;
    }
    const timer = setTimeout(
      () => void this.evictIfIdle(canonicalRoot),
      idleTtlMs,
    );
    timer.unref?.();
    this.idleTimers.set(canonicalRoot, timer);
  }

  private async evictIfIdle(canonicalRoot: string): Promise<void> {
    this.idleTimers.delete(canonicalRoot);
    const runtime = this.runtimes.get(canonicalRoot);
    if (!runtime) {
      return;
    }
    const snapshot = runtime.snapshot();
    if (
      snapshot.activeReaders > 0 ||
      snapshot.activeOperations > 0 ||
      snapshot.writerPending ||
      snapshot.watcherPending
    ) {
      this.touchRuntime(canonicalRoot);
      return;
    }
    await this.evict(canonicalRoot);
  }
}

export async function inspectRoot(
  requestedRoot: string,
  serviceOptions: CreateZvecGrepOptions = {},
  includeStatus = true,
): Promise<ZvecGrepInfoResult> {
  const canonicalRequestedRoot = await resolveRequestedRoot(
    requestedRoot,
    false,
  );
  const service = await createZvecGrep({
    ...serviceOptions,
    root: canonicalRequestedRoot,
  });
  try {
    return await service.info({ root: canonicalRequestedRoot, includeStatus });
  } finally {
    await service.close();
  }
}

export async function resolveRequestedRoot(
  requestedRoot: string,
  writable: boolean,
): Promise<string> {
  if (!isAbsolute(requestedRoot)) {
    throw new DaemonError(
      "ROOT_NOT_ABSOLUTE",
      "root must be an absolute path.",
    );
  }
  let rootStat;
  try {
    rootStat = await stat(requestedRoot);
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (code === "EACCES" || code === "EPERM") {
      throw new DaemonError("ROOT_PERMISSION_DENIED", "root is not readable.");
    }
    throw new DaemonError("ROOT_NOT_FOUND", "root does not exist.");
  }
  if (!rootStat.isDirectory()) {
    throw new DaemonError("ROOT_NOT_FOUND", "root is not a directory.");
  }
  try {
    await access(
      requestedRoot,
      writable ? fsConstants.R_OK | fsConstants.W_OK : fsConstants.R_OK,
    );
  } catch {
    throw new DaemonError(
      "ROOT_PERMISSION_DENIED",
      writable ? "root is not writable." : "root is not readable.",
    );
  }

  return realpath(requestedRoot);
}
