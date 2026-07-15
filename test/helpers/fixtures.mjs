import { execFile } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

export const cliPath = resolve("dist/cli/index.js");

export async function createTemporaryDirectory(t, prefix = "zvec-grep-test-") {
  const directory = await mkdtemp(join(tmpdir(), prefix));
  t.after(() => rm(directory, { recursive: true, force: true }));
  return directory;
}

export async function runCli(args, options = {}) {
  return execFileAsync(process.execPath, [cliPath, ...args], {
    cwd: options.cwd,
    env: {
      ...process.env,
      ...options.env,
    },
    timeout: options.timeout ?? 60_000,
    windowsHide: true,
  });
}
