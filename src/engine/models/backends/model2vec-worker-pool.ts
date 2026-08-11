import { availableParallelism } from "node:os";
import { Worker } from "node:worker_threads";
import type { EmbeddingResult } from "../embeddings.js";
import type {
  Model2VecTokenizerWorkerResponse,
  Model2VecWorkerData,
  Model2VecWorkerRequest,
  Model2VecWorkerResponse,
  SerializedWorkerError,
} from "./model2vec-runtime.js";

type WorkerJob = {
  id: number;
  texts: string[];
  tokenIds?: ArrayBuffer;
  offsets?: ArrayBuffer;
  truncated: number[];
  signal?: AbortSignal;
  resolve: (result: EmbeddingResult) => void;
  reject: (error: unknown) => void;
  settled: boolean;
  abort?: () => void;
};

type ComputeWorkerSlot = {
  worker: Worker;
  ready: boolean;
  closed: boolean;
  job?: WorkerJob;
  resolveReady: () => void;
  rejectReady: (error: unknown) => void;
  readyPromise: Promise<void>;
};

type TokenizerWorkerSlot = {
  worker: Worker;
  ready: boolean;
  closed: boolean;
  pending: number;
  resolveReady: () => void;
  rejectReady: (error: unknown) => void;
  readyPromise: Promise<void>;
};

type Model2VecWorkerUrls = {
  compute?: URL;
  tokenizer?: URL;
};

export class Model2VecWorkerPool {
  private disposed = false;
  private nextJobId = 1;
  private readonly maxWorkers: number;
  private readonly computeQueue: WorkerJob[] = [];
  private readonly jobs = new Map<number, WorkerJob>();
  private readonly computeSlots: ComputeWorkerSlot[] = [];
  private tokenizerSlot?: TokenizerWorkerSlot;
  private readonly computeWorkerUrl: URL;
  private readonly tokenizerWorkerUrl: URL;

  constructor(
    private readonly data: Model2VecWorkerData,
    maxWorkers = availableParallelism(),
    workerUrls: Model2VecWorkerUrls = {},
  ) {
    this.maxWorkers = Math.max(1, Math.floor(maxWorkers));
    this.computeWorkerUrl =
      workerUrls.compute ?? new URL("./model2vec-worker.js", import.meta.url);
    this.tokenizerWorkerUrl =
      workerUrls.tokenizer ??
      new URL("./model2vec-tokenizer-worker.js", import.meta.url);
  }

  async start(): Promise<void> {
    this.ensureNotDisposed();
    const tokenizer = this.tokenizerSlot ?? this.spawnTokenizerWorker();
    const compute =
      this.computeSlots[0] ?? this.spawnComputeWorker(undefined, false);
    await Promise.all([tokenizer.readyPromise, compute.readyPromise]);
  }

  async run(
    texts: readonly string[],
    signal?: AbortSignal,
  ): Promise<EmbeddingResult> {
    this.ensureNotDisposed();
    throwIfAborted(signal);
    const tokenizer = this.tokenizerSlot;
    if (!tokenizer?.ready || tokenizer.closed) {
      throw new Error("Model2Vec tokenizer worker pool is not ready");
    }

    return await new Promise<EmbeddingResult>((resolve, reject) => {
      const job: WorkerJob = {
        id: this.nextJobId++,
        texts: [...texts],
        truncated: [],
        signal,
        resolve,
        reject,
        settled: false,
      };
      job.abort = () => {
        if (job.settled) {
          return;
        }
        job.settled = true;
        this.jobs.delete(job.id);
        const queuedIndex = this.computeQueue.indexOf(job);
        if (queuedIndex >= 0) {
          this.computeQueue.splice(queuedIndex, 1);
        }
        reject(abortError(signal));
      };
      signal?.addEventListener("abort", job.abort, { once: true });
      this.jobs.set(job.id, job);
      tokenizer.pending++;
      tokenizer.worker.ref();
      tokenizer.worker.postMessage({ id: job.id, texts: job.texts });
      job.texts = [];
    });
  }

  async dispose(): Promise<void> {
    if (this.disposed) {
      return;
    }
    this.disposed = true;
    const error = new Error("Model2Vec worker pool is disposed");
    for (const job of this.jobs.values()) {
      this.rejectJob(job, error);
    }
    this.jobs.clear();
    this.computeQueue.length = 0;

    const workers: Worker[] = [];
    if (this.tokenizerSlot) {
      this.tokenizerSlot.closed = true;
      workers.push(this.tokenizerSlot.worker);
    }
    for (const slot of this.computeSlots) {
      slot.closed = true;
      workers.push(slot.worker);
    }
    await Promise.allSettled(workers.map((worker) => worker.terminate()));
    this.tokenizerSlot = undefined;
    this.computeSlots.length = 0;
  }

  private spawnTokenizerWorker(): TokenizerWorkerSlot {
    const worker = new Worker(this.tokenizerWorkerUrl, {
      workerData: this.data,
    });
    let resolveReady!: () => void;
    let rejectReady!: (error: unknown) => void;
    const readyPromise = new Promise<void>((resolve, reject) => {
      resolveReady = resolve;
      rejectReady = reject;
    });
    const slot: TokenizerWorkerSlot = {
      worker,
      ready: false,
      closed: false,
      pending: 0,
      resolveReady,
      rejectReady,
      readyPromise,
    };
    this.tokenizerSlot = slot;
    worker.on("message", (response: Model2VecTokenizerWorkerResponse) => {
      this.handleTokenizerMessage(slot, response);
    });
    worker.on("error", (error) => {
      this.failTokenizer(slot, error);
    });
    worker.on("exit", (code) => {
      if (!slot.closed) {
        this.failTokenizer(
          slot,
          new Error(
            `Model2Vec tokenizer worker exited unexpectedly with code ${code}`,
          ),
        );
      }
    });
    return slot;
  }

  private handleTokenizerMessage(
    slot: TokenizerWorkerSlot,
    response: Model2VecTokenizerWorkerResponse,
  ): void {
    if (slot.closed) {
      return;
    }
    if (response.type === "ready") {
      slot.ready = true;
      slot.resolveReady();
      slot.worker.unref();
      return;
    }

    slot.pending = Math.max(0, slot.pending - 1);
    if (slot.pending === 0) {
      slot.worker.unref();
    }
    const job = this.jobs.get(response.id);
    if (!job || job.settled) {
      return;
    }
    if (response.type === "error") {
      this.jobs.delete(job.id);
      this.rejectJob(job, workerError(response.error));
      return;
    }

    job.tokenIds = response.tokenIds;
    job.offsets = response.offsets;
    job.truncated = response.truncated;
    const idle = this.computeSlots.find(
      (compute) => compute.ready && !compute.closed && !compute.job,
    );
    if (idle) {
      this.dispatchCompute(idle, job);
    } else if (this.computeSlots.length < this.maxWorkers) {
      const compute = this.spawnComputeWorker(job);
      void compute.readyPromise.catch(() => undefined);
    } else {
      this.computeQueue.push(job);
    }
  }

  private spawnComputeWorker(
    job?: WorkerJob,
    handleReadyFailure = true,
  ): ComputeWorkerSlot {
    const worker = new Worker(this.computeWorkerUrl, { workerData: this.data });
    let resolveReady!: () => void;
    let rejectReady!: (error: unknown) => void;
    const readyPromise = new Promise<void>((resolve, reject) => {
      resolveReady = resolve;
      rejectReady = reject;
    });
    const slot: ComputeWorkerSlot = {
      worker,
      ready: false,
      closed: false,
      job,
      resolveReady,
      rejectReady,
      readyPromise,
    };
    this.computeSlots.push(slot);
    worker.on("message", (response: Model2VecWorkerResponse) => {
      this.handleComputeMessage(slot, response);
    });
    worker.on("error", (error) => {
      this.failCompute(slot, error);
    });
    worker.on("exit", (code) => {
      if (!slot.closed) {
        this.failCompute(
          slot,
          new Error(
            `Model2Vec compute worker exited unexpectedly with code ${code}`,
          ),
        );
      }
    });
    if (handleReadyFailure) {
      void readyPromise.catch(() => undefined);
    }
    return slot;
  }

  private handleComputeMessage(
    slot: ComputeWorkerSlot,
    response: Model2VecWorkerResponse,
  ): void {
    if (slot.closed) {
      return;
    }
    if (response.type === "ready") {
      slot.ready = true;
      slot.resolveReady();
      if (slot.job) {
        this.postComputeJob(slot, slot.job);
      } else {
        slot.worker.unref();
        this.drainCompute();
      }
      return;
    }

    const job = slot.job;
    if (!job || response.id !== job.id) {
      this.failCompute(
        slot,
        new Error("Model2Vec compute worker response is out of order"),
      );
      return;
    }
    slot.job = undefined;
    slot.worker.unref();
    this.jobs.delete(job.id);
    if (response.type === "error") {
      this.rejectJob(job, workerError(response.error));
    } else {
      try {
        const result = workerEmbeddingResult(response, this.data.dimension);
        result.truncated = job.truncated;
        this.resolveJob(job, result);
      } catch (error) {
        this.rejectJob(job, error);
        this.failCompute(slot, error);
        return;
      }
    }
    this.drainCompute();
  }

  private dispatchCompute(slot: ComputeWorkerSlot, job: WorkerJob): void {
    slot.worker.ref();
    slot.job = job;
    this.postComputeJob(slot, job);
  }

  private postComputeJob(slot: ComputeWorkerSlot, job: WorkerJob): void {
    if (job.settled) {
      slot.job = undefined;
      slot.worker.unref();
      this.drainCompute();
      return;
    }
    if (!job.tokenIds || !job.offsets) {
      this.jobs.delete(job.id);
      this.rejectJob(
        job,
        new Error("Model2Vec tokenization result is missing"),
      );
      slot.job = undefined;
      slot.worker.unref();
      this.drainCompute();
      return;
    }
    const request: Model2VecWorkerRequest = {
      id: job.id,
      tokenIds: job.tokenIds,
      offsets: job.offsets,
    };
    slot.worker.postMessage(request, [job.tokenIds, job.offsets]);
    job.tokenIds = undefined;
    job.offsets = undefined;
  }

  private drainCompute(): void {
    if (this.disposed) {
      return;
    }
    for (const slot of this.computeSlots) {
      if (!slot.ready || slot.closed || slot.job) {
        continue;
      }
      const job = this.nextComputeJob();
      if (!job) {
        return;
      }
      this.dispatchCompute(slot, job);
    }
    while (
      this.computeQueue.length > 0 &&
      this.computeSlots.length < this.maxWorkers
    ) {
      const job = this.nextComputeJob();
      if (!job) {
        return;
      }
      this.spawnComputeWorker(job);
    }
  }

  private nextComputeJob(): WorkerJob | undefined {
    while (this.computeQueue.length > 0) {
      const job = this.computeQueue.shift();
      if (job && !job.settled) {
        return job;
      }
    }
    return undefined;
  }

  private failTokenizer(slot: TokenizerWorkerSlot, error: unknown): void {
    if (slot.closed) {
      return;
    }
    slot.closed = true;
    slot.rejectReady(error);
    for (const job of this.jobs.values()) {
      this.rejectJob(job, error);
    }
    this.jobs.clear();
    this.computeQueue.length = 0;
    void slot.worker.terminate().catch(() => undefined);
  }

  private failCompute(slot: ComputeWorkerSlot, error: unknown): void {
    if (slot.closed) {
      return;
    }
    slot.closed = true;
    slot.rejectReady(error);
    if (slot.job) {
      this.jobs.delete(slot.job.id);
      this.rejectJob(slot.job, error);
      slot.job = undefined;
    }
    const index = this.computeSlots.indexOf(slot);
    if (index >= 0) {
      this.computeSlots.splice(index, 1);
    }
    void slot.worker.terminate().catch(() => undefined);
    this.drainCompute();
  }

  private resolveJob(job: WorkerJob, result: EmbeddingResult): void {
    if (job.settled) {
      return;
    }
    job.settled = true;
    job.signal?.removeEventListener("abort", job.abort!);
    job.resolve(result);
  }

  private rejectJob(job: WorkerJob, error: unknown): void {
    if (job.settled) {
      return;
    }
    job.settled = true;
    job.signal?.removeEventListener("abort", job.abort!);
    job.reject(error);
  }

  private ensureNotDisposed(): void {
    if (this.disposed) {
      throw new Error("Model2Vec worker pool is disposed");
    }
  }
}

function workerEmbeddingResult(
  response: Extract<Model2VecWorkerResponse, { type: "result" }>,
  dimension: number,
): EmbeddingResult {
  const flat = new Float32Array(response.vectors);
  if (flat.length !== response.vectorCount * dimension) {
    throw new Error("Model2Vec worker returned an invalid vector buffer");
  }
  const vectors: number[][] = [];
  for (let index = 0; index < response.vectorCount; index++) {
    vectors.push(
      Array.from(flat.subarray(index * dimension, (index + 1) * dimension)),
    );
  }
  return { vectors, truncated: response.truncated };
}

function workerError(serialized: SerializedWorkerError): Error {
  const error = new Error(serialized.message);
  error.name = serialized.name ?? "Error";
  if (serialized.stack) {
    error.stack = serialized.stack;
  }
  return error;
}

function throwIfAborted(signal?: AbortSignal): void {
  if (signal?.aborted) {
    throw abortError(signal);
  }
}

function abortError(signal?: AbortSignal): Error {
  return signal?.reason instanceof Error
    ? signal.reason
    : new Error("Model2Vec embedding was cancelled");
}
