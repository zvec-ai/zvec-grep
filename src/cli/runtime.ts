import { spawn } from "node:child_process";
import type { ParsedArgs } from "./types.js";

export const LIFTOFF_ONLY_FLAG = "--liftoff-only";

export function commandNeedsLiftoffOnly(parsed: ParsedArgs): boolean {
  if (parsed.command === "index") {
    return parsed.options.drop !== true;
  }
  if (parsed.command === "query") {
    return parsed.options.rg !== true;
  }
  return parsed.command === "server" && parsed.options.serverAction === "run";
}

export function liftoffOnlyEnabled(
  execArgv: readonly string[] = process.execArgv,
): boolean {
  return execArgv.includes(LIFTOFF_ONLY_FLAG);
}

export function liftoffOnlyNodeArgs(
  cliPath: string,
  args: readonly string[],
  execArgv: readonly string[] = process.execArgv,
): string[] {
  return [LIFTOFF_ONLY_FLAG, ...execArgv, cliPath, ...args];
}

export async function restartWithLiftoffOnlyIfNeeded(
  parsed: ParsedArgs,
  args: readonly string[],
): Promise<boolean> {
  if (!commandNeedsLiftoffOnly(parsed) || liftoffOnlyEnabled()) {
    return false;
  }

  const cliPath = process.argv[1];
  if (!cliPath) {
    throw new Error("Cannot restart zg because the CLI path is unavailable");
  }
  const nodeArgs = liftoffOnlyNodeArgs(cliPath, args);

  // V8 reads --liftoff-only during process startup. Replacing the process keeps
  // the same PID, stdio, and signal handling while applying the flag early enough.
  if (process.execve) {
    process.execve(
      process.execPath,
      [process.execPath, ...nodeArgs],
      process.env,
    );
  }

  // process.execve is unavailable on Windows and Node 22 releases before 22.15.
  const exitCode = await runReplacementChild(nodeArgs);
  process.exitCode = exitCode;
  return true;
}

async function runReplacementChild(
  nodeArgs: readonly string[],
): Promise<number> {
  return await new Promise<number>((resolve, reject) => {
    const child = spawn(process.execPath, nodeArgs, {
      env: process.env,
      stdio: "inherit",
      windowsHide: true,
    });
    child.once("error", reject);
    child.once("exit", (code) => resolve(code ?? 1));
  });
}
