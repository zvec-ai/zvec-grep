import { runPortabilityWorker } from "./portability-worker-runner.js";
import { EngineError, redactErrorText } from "../engine/errors.js";
import type {
  PortabilityInput,
  PortabilityOperation,
  PortabilityWorkerMessage,
} from "./portability-operation.js";

if (!process.send)
  throw new Error("Portability process requires an IPC parent.");
const controller = new AbortController();
let resume: (() => void) | undefined;
let started = false;
function send(message: PortabilityWorkerMessage): Promise<void> {
  return new Promise((resolve, reject) => {
    if (!process.connected) {
      reject(new Error("Portability parent disconnected"));
      return;
    }
    process.send!(message, (error) => (error ? reject(error) : resolve()));
  });
}
process.on("disconnect", () => {
  controller.abort();
  resume?.();
});
process.on(
  "message",
  (message: {
    type: string;
    operation: PortabilityOperation;
    input: PortabilityInput;
  }) => {
    if (message.type === "cancel") {
      controller.abort();
      resume?.();
    } else if (message.type === "continue") resume?.();
    else if (message.type === "start" && !started) {
      started = true;
      void run(message.operation, message.input);
    }
  },
);
async function run(operation: PortabilityOperation, input: PortabilityInput) {
  try {
    const result = await runPortabilityWorker(operation, input, {
      signal: controller.signal,
      onProgress: async (stage, detail) => {
        const acknowledged = new Promise<void>((resolve) => {
          resume = resolve;
        });
        await send({ type: "progress", stage, detail });
        if (controller.signal.aborted) resume?.();
        await acknowledged;
        resume = undefined;
      },
    });
    await send({ type: "result", result });
  } catch (error) {
    const cause = error instanceof Error ? error : new Error(String(error));
    if (process.connected)
      await send({
        type: "error",
        error: {
          code:
            error instanceof EngineError
              ? error.code
              : "code" in cause && typeof cause.code === "string"
                ? cause.code
                : "ZVEC_GREP.ENGINE.TRANSFER_FAILED",
          message: redactErrorText(cause.message, 8192),
          ...("context" in cause && typeof cause.context === "string"
            ? { context: redactErrorText(cause.context, 8192) }
            : {}),
        },
      });
  } finally {
    if (process.connected) process.disconnect();
  }
}
