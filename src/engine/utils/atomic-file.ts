import { randomUUID } from "node:crypto";
import { rename, unlink, writeFile } from "node:fs/promises";

const REPLACE_RETRY_DELAYS_MS = [5, 10, 20, 40, 80, 160, 320] as const;

export async function replaceFileAtomically(
  path: string,
  contents: string,
  options: { mode?: number } = {},
): Promise<void> {
  const temporaryPath = `${path}.${process.pid}.${randomUUID()}.tmp`;
  try {
    await writeFile(temporaryPath, contents, {
      encoding: "utf8",
      mode: options.mode,
      flag: "wx",
    });
    await renameWithTransientRetries(temporaryPath, path);
  } finally {
    await unlink(temporaryPath).catch(() => undefined);
  }
}

async function renameWithTransientRetries(
  source: string,
  destination: string,
): Promise<void> {
  for (const delayMs of REPLACE_RETRY_DELAYS_MS) {
    try {
      await rename(source, destination);
      return;
    } catch (error) {
      if (!isTransientReplacementError(error)) throw error;
      await new Promise((resolve) => setTimeout(resolve, delayMs));
    }
  }
  await rename(source, destination);
}

function isTransientReplacementError(error: unknown): boolean {
  const code = (error as NodeJS.ErrnoException | undefined)?.code;
  return (
    code === "EACCES" ||
    code === "EBUSY" ||
    code === "EEXIST" ||
    code === "EPERM"
  );
}
