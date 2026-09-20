import { availableParallelism } from "node:os";
import { Worker } from "node:worker_threads";
import type { EmbeddingResult } from "../embeddings.js";
import type {
  Model2VecWorkerData,
  Model2VecWorkerRequest,
  Model2VecWorkerResponse,
  SerializedWorkerError,
} from "./model2vec-runtime.js";

type WorkerJob = {
  id: number;
  texts: string[];
  signal?: AbortSignal;
  resolve: (result: EmbeddingResult) => void;
  reject: (error: unknown) => void;
  settled: boolean;
  abort?: () => void;
};

type WorkerSlot = {
  worker: Worker;
  ready: boolean;
  closed: boolean;
  job?: WorkerJob;
  resolveReady: () => void;
  rejectReady: (error: unknown) => void;
  readyPromise: Promise<void>;
};

export class Model2VecWorkerPool {
  private disposed = false;
  private nextJobId = 1;
  private readonly maxWorkers: number;
  private readonly queue: WorkerJob[] = [];
  private readonly slots: WorkerSlot[] = [];

  constructor(
    private readonly data: Model2VecWorkerData,
    maxWorkers = availableParallelism(),
    private readonly workerUrl = new URL(
      "./model2vec-worker.js",
      import.meta.url,
    ),
  ) {
    this.maxWorkers = Math.max(1, Math.floor(maxWorkers));
  }

  async start(): Promise<void> {
    this.ensureNotDisposed();
    if (this.slots.length === 0) {
      await this.spawnWorker().readyPromise;
    }
  }

  async run(
    texts: readonly string[],
    signal?: AbortSignal,
  ): Promise<EmbeddingResult> {
    this.ensureNotDisposed();
    throwIfAborted(signal);

    return await new Promise<EmbeddingResult>((resolve, reject) => {
      const job: WorkerJob = {
        id: this.nextJobId++,
        texts: [...texts],
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
        const queuedIndex = this.queue.indexOf(job);
        if (queuedIndex >= 0) {
          this.queue.splice(queuedIndex, 1);
        }
        reject(abortError(signal));
      };
      signal?.addEventListener("abort", job.abort, { once: true });

      const idle = this.slots.find(
        (slot) => slot.ready && !slot.closed && !slot.job,
      );
      if (idle) {
        this.dispatch(idle, job);
      } else if (this.slots.length < this.maxWorkers) {
        const slot = this.spawnWorker(job);
        void slot.readyPromise.catch(() => undefined);
      } else {
        this.queue.push(job);
      }
    });
  }

  async dispose(): Promise<void> {
    if (this.disposed) {
      return;
    }
    this.disposed = true;
    const error = new Error("Model2Vec worker pool is disposed");
    for (const job of this.queue.splice(0)) {
      this.rejectJob(job, error);
    }
    for (const slot of this.slots) {
      if (slot.job) {
        this.rejectJob(slot.job, error);
        slot.job = undefined;
      }
      slot.closed = true;
    }
    await Promise.allSettled(this.slots.map((slot) => slot.worker.terminate()));
    this.slots.length = 0;
  }

  private spawnWorker(job?: WorkerJob): WorkerSlot {
    const worker = new Worker(this.workerUrl, { workerData: this.data });
    let resolveReady!: () => void;
    let rejectReady!: (error: unknown) => void;
    const readyPromise = new Promise<void>((resolve, reject) => {
      resolveReady = resolve;
      rejectReady = reject;
    });
    const slot: WorkerSlot = {
      worker,
      ready: false,
      closed: false,
      job,
      resolveReady,
      rejectReady,
      readyPromise,
    };
    this.slots.push(slot);
    worker.on("message", (response: Model2VecWorkerResponse) => {
      this.handleMessage(slot, response);
    });
    worker.on("error", (error) => {
      this.failSlot(slot, error);
    });
    worker.on("exit", (code) => {
      if (!slot.closed) {
        this.failSlot(
          slot,
          new Error(`Model2Vec worker exited unexpectedly with code ${code}`),
        );
      }
    });
    return slot;
  }

  private handleMessage(
    slot: WorkerSlot,
    response: Model2VecWorkerResponse,
  ): void {
    if (slot.closed) {
      return;
    }
    if (response.type === "ready") {
      slot.ready = true;
      slot.resolveReady();
      if (slot.job) {
        this.postJob(slot, slot.job);
      } else {
        slot.worker.unref();
        this.drain();
      }
      return;
    }

    const job = slot.job;
    if (!job || response.id !== job.id) {
      this.failSlot(
        slot,
        new Error("Model2Vec worker response is out of order"),
      );
      return;
    }
    slot.job = undefined;
    slot.worker.unref();
    if (response.type === "error") {
      this.rejectJob(job, workerError(response.error));
    } else {
      try {
        this.resolveJob(
          job,
          workerEmbeddingResult(response, this.data.dimension),
        );
      } catch (error) {
        this.rejectJob(job, error);
        this.failSlot(slot, error);
        return;
      }
    }
    this.drain();
  }

  private dispatch(slot: WorkerSlot, job: WorkerJob): void {
    slot.worker.ref();
    slot.job = job;
    this.postJob(slot, job);
  }

  private postJob(slot: WorkerSlot, job: WorkerJob): void {
    if (job.settled) {
      slot.job = undefined;
      slot.worker.unref();
      this.drain();
      return;
    }
    slot.worker.postMessage({
      id: job.id,
      texts: job.texts,
    } satisfies Model2VecWorkerRequest);
  }

  private drain(): void {
    if (this.disposed) {
      return;
    }
    for (const slot of this.slots) {
      if (!slot.ready || slot.closed || slot.job) {
        continue;
      }
      const job = this.nextQueuedJob();
      if (!job) {
        return;
      }
      this.dispatch(slot, job);
    }
    while (this.queue.length > 0 && this.slots.length < this.maxWorkers) {
      const job = this.nextQueuedJob();
      if (!job) {
        return;
      }
      const slot = this.spawnWorker(job);
      void slot.readyPromise.catch(() => undefined);
    }
  }

  private nextQueuedJob(): WorkerJob | undefined {
    while (this.queue.length > 0) {
      const job = this.queue.shift();
      if (job && !job.settled) {
        return job;
      }
    }
    return undefined;
  }

  private failSlot(slot: WorkerSlot, error: unknown): void {
    if (slot.closed) {
      return;
    }
    slot.closed = true;
    slot.rejectReady(error);
    if (slot.job) {
      this.rejectJob(slot.job, error);
      slot.job = undefined;
    }
    const index = this.slots.indexOf(slot);
    if (index >= 0) {
      this.slots.splice(index, 1);
    }
    void slot.worker.terminate().catch(() => undefined);
    this.drain();
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
