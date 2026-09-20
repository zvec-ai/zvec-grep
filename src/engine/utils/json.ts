import { randomUUID } from "node:crypto";
import {
  chmodSync,
  mkdirSync,
  readFileSync,
  renameSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import {
  chmod,
  mkdir,
  readFile,
  rename,
  unlink,
  writeFile,
} from "node:fs/promises";
import { dirname } from "node:path";

export type JsonWriteOptions = {
  directoryMode?: number;
  fileMode?: number;
};

export async function readJsonFile<T>(path: string, fallback: T): Promise<T> {
  try {
    const text = await readFile(path, "utf8");
    return JSON.parse(text) as T;
  } catch (error) {
    if (isNodeError(error) && error.code === "ENOENT") {
      return fallback;
    }

    throw error;
  }
}

export function readJsonFileSync<T>(path: string, fallback: T): T {
  try {
    const text = readFileSync(path, "utf8");
    return JSON.parse(text) as T;
  } catch (error) {
    if (isNodeError(error) && error.code === "ENOENT") {
      return fallback;
    }

    throw error;
  }
}

export async function writeJsonFile(
  path: string,
  value: unknown,
  options: JsonWriteOptions = {},
): Promise<void> {
  const directory = dirname(path);
  await mkdir(directory, { recursive: true, mode: options.directoryMode });
  if (options.directoryMode !== undefined) {
    await chmod(directory, options.directoryMode);
  }

  const tmpPath = `${path}.${process.pid}.${randomUUID()}.tmp`;
  const text = `${JSON.stringify(value, null, 2)}\n`;

  try {
    await writeFile(tmpPath, text, {
      encoding: "utf8",
      mode: options.fileMode,
    });
    if (options.fileMode !== undefined) {
      await chmod(tmpPath, options.fileMode);
    }
    await rename(tmpPath, path);
  } catch (error) {
    await unlink(tmpPath).catch(() => undefined);
    throw error;
  }
}

export function writeJsonFileSync(
  path: string,
  value: unknown,
  options: JsonWriteOptions = {},
): void {
  const directory = dirname(path);
  mkdirSync(directory, { recursive: true, mode: options.directoryMode });
  if (options.directoryMode !== undefined) {
    chmodSync(directory, options.directoryMode);
  }

  const tmpPath = `${path}.${process.pid}.${randomUUID()}.tmp`;
  const text = `${JSON.stringify(value, null, 2)}\n`;

  try {
    writeFileSync(tmpPath, text, {
      encoding: "utf8",
      mode: options.fileMode,
    });
    if (options.fileMode !== undefined) {
      chmodSync(tmpPath, options.fileMode);
    }
    renameSync(tmpPath, path);
  } catch (error) {
    try {
      unlinkSync(tmpPath);
    } catch {
      // The temporary file may not have been created.
    }
    throw error;
  }
}

function isNodeError(error: unknown): error is NodeJS.ErrnoException {
  return typeof error === "object" && error !== null && "code" in error;
}
