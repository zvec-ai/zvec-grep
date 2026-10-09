import { parentPort, workerData } from "node:worker_threads";
import { migrateWorkspaceIndex } from "../engine/migrate/index.js";
import {
  exportWorkspaceIndex,
  importWorkspaceIndex,
} from "../engine/transfer/index.js";
import { EngineError, redactErrorText } from "../engine/errors.js";
import type {
  PortabilityWorkerData,
  PortabilityWorkerMessage,
} from "./portability-operation.js";

const port = parentPort;
if (!port) throw new Error("Index transfer worker requires a parent port.");
const {
  operation,
  input,
  control: buffer,
} = workerData as PortabilityWorkerData;
const control = new Int32Array(buffer);
function checkCancelled() {
  if (Atomics.load(control, 0) !== 0) {
    throw new EngineError("Index operation cancelled before publication.", {
      code: "ZVEC_GREP.ENGINE.OPERATION_CANCELLED",
    });
  }
}
function post(message: PortabilityWorkerMessage) {
  port!.postMessage(message);
}
function onProgress(stage: string, detail: string) {
  // The engine has committed at 'done'. Late cancellation cannot undo it.
  if (stage !== "done") checkCancelled();
  Atomics.store(control, 1, 0);
  post({ type: "progress", stage, detail });
  while (Atomics.load(control, 1) === 0) {
    if (stage !== "done") checkCancelled();
    Atomics.wait(control, 1, 0, 1_000);
  }
  if (stage !== "done") checkCancelled();
}
try {
  checkCancelled();
  const result =
    operation === "migrate"
      ? await migrateWorkspaceIndex({
          sourceHome: input.sourceHome!,
          destinationRoot: input.destinationRoot!,
          verifySampleLimit: input.verifySampleLimit ?? 0,
          onProgress,
        })
      : operation === "export"
        ? await exportWorkspaceIndex({
            sourceHome: input.sourceHome!,
            artifactPath: input.artifactPath!,
            onProgress,
          })
        : await importWorkspaceIndex({
            artifactPath: input.artifactPath!,
            destinationRoot: input.destinationRoot!,
            verifySampleLimit: input.verifySampleLimit ?? 0,
            onProgress,
          });
  post({ type: "result", result });
} catch (error) {
  const cause = error instanceof Error ? error : new Error(String(error));
  post({
    type: "error",
    error: {
      code:
        error instanceof EngineError
          ? error.code
          : "ZVEC_GREP.ENGINE.TRANSFER_FAILED",
      message: redactErrorText(cause.message, 8_192),
      ...(error instanceof EngineError && error.context
        ? { context: redactErrorText(error.context, 8_192) }
        : {}),
    },
  });
} finally {
  port.close();
}
