import type { ParsedArgs } from "./types.js";

export function handleStdoutError(error: NodeJS.ErrnoException): void {
  if (error.code === "EPIPE") {
    return;
  }
  throw error;
}

export function installStdoutErrorHandler(parsed: ParsedArgs): void {
  // Long-lived transports own their shutdown and resource cleanup.
  if (
    parsed.command === "server" &&
    (parsed.options.serverStdio || parsed.options.serverAction === "run")
  ) {
    return;
  }
  process.stdout.on("error", (error) => {
    handleStdoutError(error);
  });
}
