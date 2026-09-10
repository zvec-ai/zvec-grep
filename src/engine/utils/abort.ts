/** Stop waiting without losing ownership of an operation that ignores abort.
 * Callers still track/drain that operation and release its resources on settle. */
export function awaitWithSignal<T>(
  operation: Promise<T>,
  signal?: AbortSignal,
): Promise<T> {
  if (!signal) return operation;
  return new Promise<T>((resolve, reject) => {
    const rejectUnchanged = (reason: unknown) => {
      // AbortSignal.reason and the original rejection may be arbitrary values.
      // eslint-disable-next-line @typescript-eslint/prefer-promise-reject-errors
      reject(reason);
    };
    const onAbort = () => rejectUnchanged(signal.reason);
    // Observe both outcomes even if the signal was already aborted. A late
    // rejection must not become unhandled after the foreground request leaves.
    void operation.then(
      (value) => {
        signal.removeEventListener("abort", onAbort);
        if (signal.aborted) rejectUnchanged(signal.reason);
        else resolve(value);
      },
      (error: unknown) => {
        signal.removeEventListener("abort", onAbort);
        rejectUnchanged(signal.aborted ? signal.reason : error);
      },
    );
    if (signal.aborted) onAbort();
    else signal.addEventListener("abort", onAbort, { once: true });
  });
}
