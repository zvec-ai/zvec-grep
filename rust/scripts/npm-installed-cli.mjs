import { accessSync, mkdirSync, writeFileSync } from "node:fs";
import { spawn, spawnSync } from "node:child_process";
import { delimiter, join, resolve } from "node:path";

const processes = new WeakMap();
const cleanupTimeout = 5_000;

async function within(promise, timeout) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((resolve) => {
      timer = setTimeout(() => resolve(undefined), timeout);
    })]);
  } finally { clearTimeout(timer); }
}

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
  // Unix children share a dedicated process group. Windows taskkill /T follows
  // the npm PowerShell shim's descendants while the shim is still alive.
  const child = spawn(invocation.command, invocation.args, {
    ...options, detached: process.platform !== "win32", windowsHide: true,
  });
  const state = { error: undefined, closed: false };
  state.completion = new Promise((resolve) => {
    child.once("error", (error) => { state.error = error; });
    child.once("close", (code, signal) => {
      state.closed = true;
      resolve({ code, signal });
    });
  });
  processes.set(child, state);
  return child;
}

export async function terminateInstalledCli(child) {
  const state = processes.get(child);
  if (!state) throw new Error("Cannot clean up an unmanaged npm command");
  if (state.closed) return;
  try {
    if (child.pid) {
      if (process.platform === "win32") {
        const result = spawnSync(join(process.env.SystemRoot ?? "C:\\Windows", "System32", "taskkill.exe"),
          ["/PID", String(child.pid), "/T", "/F"], {
            encoding: "utf8", windowsHide: true, timeout: cleanupTimeout,
          });
        if (result.error) throw result.error;
        if (result.status !== 0 && !await within(state.completion, cleanupTimeout)) {
          throw new Error(`Unable to terminate npm process tree: ${result.stderr}`);
        }
      } else {
        try { process.kill(-child.pid, "SIGKILL"); }
        catch (error) { if (error.code !== "ESRCH") throw error; }
      }
    }
    if (!await within(state.completion, cleanupTimeout)) {
      throw new Error("npm process tree did not close after termination");
    }
  } finally {
    // A cleanup failure must fail the smoke without holding the test runner open.
    if (!state.closed) {
      child.stdin?.destroy();
      child.stdout?.destroy();
      child.stderr?.destroy();
      child.unref();
    }
  }
}

export async function closeInstalledCli(child, { timeout = cleanupTimeout } = {}) {
  const state = processes.get(child);
  child.stdin.end();
  const result = await within(state.completion, timeout);
  if (!result) {
    await terminateInstalledCli(child);
    throw new Error("npm MCP entry did not exit after stdin closed");
  }
  if (state.error) throw state.error;
  if (result.code !== 0) throw new Error(`npm MCP entry exited with ${result.signal ?? result.code}`);
}

export async function runInstalledCli(prefix, args, { logRoot, label, timeout = 30_000,
  maxBuffer = 16 * 1024 * 1024, ...options } = {}) {
  if (logRoot && !/^[a-z0-9-]+$/.test(label)) throw new Error("CLI evidence requires a safe stage name");
  const child = spawnInstalledCli(prefix, args, { ...options, stdio: "pipe" });
  const state = processes.get(child);
  const output = { stdout: [], stderr: [] };
  let bytes = 0;
  let failure;
  let termination;
  let aborted;
  const stopped = new Promise((resolve) => { aborted = resolve; });
  const abort = (error) => {
    if (termination) return;
    failure = error;
    termination = terminateInstalledCli(child).catch((cleanup) => {
      failure = new AggregateError([error, cleanup], `${error.message}; ${cleanup.message}`);
    }).finally(aborted);
  };
  for (const stream of ["stdout", "stderr"]) {
    child[stream].on("data", (data) => {
      const remaining = Math.max(0, maxBuffer - bytes);
      if (remaining) output[stream].push(data.subarray(0, remaining));
      bytes += data.length;
      if (bytes > maxBuffer) abort(new Error("npm zg output exceeded maxBuffer"));
    });
  }
  child.stdin.on("error", abort);
  child.stdin.end();
  const timer = setTimeout(() => abort(new Error(`npm zg ${label ?? args[0]} timed out after ${timeout}ms`)), timeout);
  const result = await Promise.race([state.completion, stopped]);
  clearTimeout(timer);
  if (termination) await termination;
  const stdout = Buffer.concat(output.stdout).toString("utf8");
  const stderr = Buffer.concat(output.stderr).toString("utf8");
  if (logRoot) {
    mkdirSync(logRoot, { recursive: true });
    writeFileSync(join(logRoot, `${label}.stdout`), stdout);
    writeFileSync(join(logRoot, `${label}.stderr`), stderr);
  }
  if (failure) throw failure;
  if (state.error) throw state.error;
  if (result.code !== 0) {
    throw new Error(`npm zg ${label ?? args[0]} failed (${result.signal ?? result.code}):\n${stderr.slice(-8000)}`);
  }
  return { stdout, stderr };
}
