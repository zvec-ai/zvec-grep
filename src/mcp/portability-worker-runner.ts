import { Worker } from "node:worker_threads";
import { setImmediate } from "node:timers/promises";
import type {
  PortabilityOperation,
  PortabilityInput,
  PortabilityResult,
  PortabilityWorkerData,
  PortabilityWorkerMessage,
} from "./portability-operation.js";

// Native database scans are synchronous. Keep them off the MCP event loop.
// Cancellation asks the worker to unwind through the engine's own finalizers;
// never terminate a worker while it owns a destination reservation.
export async function runPortabilityWorker(
  operation: PortabilityOperation,
  input: PortabilityInput,
  options: {
    signal: AbortSignal;
    onProgress: (stage: string, detail: string) => Promise<void>;
  },
): Promise<PortabilityResult> {
  options.signal.throwIfAborted();
  const control = new Int32Array(new SharedArrayBuffer(8));
  const worker = new Worker(
    new URL("./portability-worker.js", import.meta.url),
    {
      workerData: {
        operation,
        input,
        control: control.buffer,
      } satisfies PortabilityWorkerData,
    },
  );
  const cancel = () => {
    Atomics.store(control, 0, 1);
    Atomics.notify(control, 1);
  };
  options.signal.addEventListener("abort", cancel, { once: true });
  if (options.signal.aborted) cancel();
  try {
    return await new Promise<PortabilityResult>((resolve, reject) => {
      let result: PortabilityResult | undefined;
      let failure: Error | undefined;
      let progress = Promise.resolve();
      worker.on("message", (message: PortabilityWorkerMessage) => {
        if (message.type === "progress") {
          progress = progress.then(async () => {
            try {
              await options.onProgress(message.stage, message.detail);
              // Let cancellation notifications run before the worker continues.
              await setImmediate();
            } catch (error) {
              failure =
                error instanceof Error ? error : new Error(String(error));
              cancel();
            } finally {
              Atomics.store(control, 1, 1);
              Atomics.notify(control, 1);
            }
          });
        } else if (message.type === "result") {
          result = message.result;
        } else {
          failure = Object.assign(
            new Error(message.error.message),
            message.error,
          );
        }
      });
      worker.on("error", (error) => {
        failure = error;
      });
      worker.on("exit", (code) => {
        void progress.then(() => {
          if (failure) reject(failure);
          else if (code !== 0 || result === undefined) {
            reject(
              new Error(
                `Index ${operation} worker exited ${code} without a completed result.`,
              ),
            );
          } else resolve(result);
        });
      });
    });
  } finally {
    options.signal.removeEventListener("abort", cancel);
  }
}
