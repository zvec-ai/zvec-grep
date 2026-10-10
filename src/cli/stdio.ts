type Exit = (code: number) => never;

export function handleStdoutError(
  error: NodeJS.ErrnoException,
  exit: Exit = process.exit,
): void {
  if (error.code === "EPIPE") {
    exit(0);
  }
  throw error;
}

export function installStdoutErrorHandler(): void {
  process.stdout.on("error", (error) => {
    handleStdoutError(error);
  });
  process.on("SIGPIPE", () => process.exit(0));
}
