export class DaemonError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly retryable = false,
  ) {
    super(`[${code}] ${message}`);
    this.name = "DaemonError";
  }
}
