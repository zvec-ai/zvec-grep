import { fork, type Serializable } from "node:child_process";
import { createTransferScratch } from "../engine/storage/transfer-scratch.js";
import { EngineError, redactErrorText } from "../engine/errors.js";
import { setImmediate } from "node:timers/promises";
import type { MigrateWorkspaceIndexResult } from "../engine/migrate/index.js";
import type {
  ExportWorkspaceIndexResult,
  ImportWorkspaceIndexResult,
} from "../engine/transfer/index.js";

export type PortabilityOperation = "migrate" | "export" | "import";
export type PortabilityInput = {
  confirm: true;
  /** Internal caller policy; MCP always compares all vectors. */
  verifySampleLimit?: number;
  sourceHome?: string;
  artifactPath?: string;
  destinationRoot?: string;
};
export type PortabilityResults = {
  migrate: MigrateWorkspaceIndexResult;
  export: ExportWorkspaceIndexResult;
  import: ImportWorkspaceIndexResult;
};
export type PortabilityResult = PortabilityResults[PortabilityOperation];
export type PortabilityError = {
  code: string;
  message: string;
  context?: string;
};
export type PortabilityWorkerData = {
  operation: PortabilityOperation;
  input: PortabilityInput;
  control: SharedArrayBuffer;
};
export type PortabilityWorkerMessage =
  | { type: "progress"; stage: string; detail: string }
  | { type: "result"; result: PortabilityResult }
  | { type: "error"; error: PortabilityError };

// The native binding can abort the process on allocation failure. A worker
// thread protects responsiveness, not the daemon's address space. Run it in a
// child process; retain cooperative cancellation and the engine's finalizers.
export async function runPortabilityOperation<T extends PortabilityOperation>(
  operation: T,
  input: PortabilityInput,
  options: {
    signal: AbortSignal;
    onProgress: (stage: string, detail: string) => Promise<void>;
    onScratch?: (path: string) => void;
  },
): Promise<PortabilityResults[T]> {
  options.signal.throwIfAborted();
  const scratch = createTransferScratch();
  try {
    options.onScratch?.(scratch.path);
    options.signal.throwIfAborted();
    const child = fork(
      new URL("./portability-process.js", import.meta.url),
      [],
      {
        stdio: ["ignore", "ignore", "pipe", "ipc"],
        execArgv: process.execArgv.filter(
          (arg) => !arg.startsWith("--input-type"),
        ),
        env: {
          ...process.env,
          TMPDIR: scratch.path,
          TEMP: scratch.path,
          TMP: scratch.path,
        },
      },
    );
    const send = (message: Serializable) => {
      if (child.connected)
        child.send(message, () => {
          /* Exit handles channel failure. */
        });
    };
    const cancel = () => send({ type: "cancel" });
    options.signal.addEventListener("abort", cancel, { once: true });
    try {
      return await new Promise<PortabilityResults[T]>((resolve, reject) => {
        let result: PortabilityResult | undefined;
        let failure: Error | undefined;
        let stderr = "";
        let progress = Promise.resolve();
        child.stderr!.setEncoding("utf8");
        child.stderr!.on("data", (chunk: string) => {
          stderr = (stderr + chunk).slice(-8192);
        });
        child.on("message", (message: PortabilityWorkerMessage) => {
          if (message.type === "progress") {
            progress = progress.then(async () => {
              try {
                await options.onProgress(message.stage, message.detail);
                await setImmediate();
              } catch (error) {
                failure =
                  error instanceof Error ? error : new Error(String(error));
                cancel();
              } finally {
                send({ type: "continue" });
              }
            });
          } else if (message.type === "result") result = message.result;
          else
            failure = Object.assign(
              new Error(message.error.message),
              message.error,
            );
        });
        child.on("error", (error) => {
          failure = error;
        });
        child.on("close", (code, signal) => {
          void progress.then(() => {
            if (failure) reject(failure);
            else if (code !== 0 || result === undefined)
              reject(
                new EngineError(
                  `Index ${operation} process exited ${signal ?? code} without a completed result. The caller is still running. Inspect the destination for INCOMPLETE state before retrying; do not remove source data.`,
                  {
                    code: "ZVEC_GREP.ENGINE.TRANSFER_PROCESS_FAILED",
                    context: redactErrorText(stderr, 8192),
                  },
                ),
              );
            else resolve(result as PortabilityResults[T]);
          });
        });
        try {
          if (child.pid !== undefined) scratch.setChildPid(child.pid);
          send({ type: "start", operation, input });
        } catch (error) {
          failure = error instanceof Error ? error : new Error(String(error));
          child.kill();
        }
        if (options.signal.aborted) cancel();
      });
    } finally {
      options.signal.removeEventListener("abort", cancel);
    }
  } finally {
    // Only this process's private snapshot area is removed. A crash can leave
    // an INCOMPLETE destination; it remains blocked for checked operator recovery.
    scratch.cleanup();
  }
}
