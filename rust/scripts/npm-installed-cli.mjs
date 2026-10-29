import { accessSync, mkdirSync, writeFileSync } from "node:fs";
import { spawn, spawnSync } from "node:child_process";
import { delimiter, join, resolve } from "node:path";

// Preserve the npm entry path: realpath() here would hide the macOS symlink bug.
export function installedCliInvocation(prefix, args, platform = process.platform) {
  const entry = platform === "win32"
    ? join(resolve(prefix), "zg.ps1")
    : join(resolve(prefix), "bin", "zg");
  return platform === "win32"
    ? { entry, command: "pwsh", args: ["-NoLogo", "-NoProfile", "-NonInteractive", "-File", entry, ...args] }
    : { entry, command: entry, args };
}

export function installedCliEnvironment(prefix, home, overrides = {}) {
  const env = { ...process.env };
  for (const key of Object.keys(env)) {
    if (/^(ZVEC_GREP_|ZG_)/.test(key) ||
        ["ORT_DYLIB_PATH", "GGML_BACKEND_PATH", "LD_LIBRARY_PATH", "LD_PRELOAD",
          "DYLD_LIBRARY_PATH", "DYLD_FALLBACK_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES"].includes(key)) {
      delete env[key];
    }
  }
  const pathKey = Object.keys(env).find((key) => key.toLowerCase() === "path") ?? "PATH";
  env[pathKey] = `${process.platform === "win32" ? resolve(prefix) : join(resolve(prefix), "bin")}${delimiter}${env[pathKey] ?? ""}`;
  return { ...env, ZVEC_GREP_HOME: resolve(home), ZVEC_GREP_MODE: "direct", NO_COLOR: "1", ...overrides };
}

export function spawnInstalledCli(prefix, args, options) {
  const invocation = installedCliInvocation(prefix, args);
  accessSync(invocation.entry); // A missing shim must never fall through to another zg on PATH.
  return spawn(invocation.command, invocation.args, options);
}

export function runInstalledCli(prefix, args, { logRoot, label, timeout = 30_000, ...options } = {}) {
  const invocation = installedCliInvocation(prefix, args);
  accessSync(invocation.entry);
  const result = spawnSync(invocation.command, invocation.args, {
    encoding: "utf8", maxBuffer: 16 * 1024 * 1024, timeout, ...options,
  });
  if (logRoot) {
    if (!/^[a-z0-9-]+$/.test(label)) throw new Error("CLI evidence requires a safe stage name");
    mkdirSync(logRoot, { recursive: true });
    writeFileSync(join(logRoot, `${label}.stdout`), result.stdout ?? "");
    writeFileSync(join(logRoot, `${label}.stderr`), result.stderr ?? "");
  }
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`npm zg ${label ?? args[0]} failed (${result.signal ?? result.status}):\n${(result.stderr ?? "").slice(-8000)}`);
  }
  return { stdout: result.stdout, stderr: result.stderr };
}
