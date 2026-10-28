import { spawn } from "node:child_process";
import { createWriteStream } from "node:fs";
import { appendFile, readdir, readFile } from "node:fs/promises";
import { finished } from "node:stream/promises";
import { join } from "node:path";
import { performance } from "node:perf_hooks";
import { stripVTControlCharacters } from "node:util";
import {
  ResourceMonitor,
  directoryBytes,
} from "../zg-retrieval/core/performance.mjs";
import { writeJson } from "../zg-retrieval/core/io.mjs";

// Avoid launching ps ten times a second during multi-hour builds. Read only the
// product trees on Linux; the shared monitor supplies the portable fallback.
export async function linuxTreeRss(roots) {
  const visited = new Set();
  let bytes = 0;
  async function visit(pid) {
    if (!Number.isInteger(pid) || pid <= 0 || visited.has(pid)) return;
    visited.add(pid);
    try {
      const [status, tasks] = await Promise.all([
        readFile(`/proc/${pid}/status`, "utf8"),
        readdir(`/proc/${pid}/task`),
      ]);
      const rss = /^VmRSS:\s+(\d+) kB$/m.exec(status);
      if (rss) bytes += Number(rss[1]) * 1024;
      // Children can belong to any thread, not only the thread-group leader.
      for (const tid of tasks) {
        const children = await readFile(
          `/proc/${pid}/task/${tid}/children`,
          "utf8",
        ).catch((error) => {
          if (error.code === "ENOENT" || error.code === "ESRCH") return "";
          throw error;
        });
        for (const child of children.trim().split(/\s+/))
          if (child) await visit(Number(child));
      }
    } catch (error) {
      if (error.code !== "ENOENT" && error.code !== "ESRCH") throw error;
    }
  }
  for (const pid of roots) await visit(pid);
  return bytes || null;
}

export class DurableMonitor extends ResourceMonitor {
  constructor(roots, path, { interval = 1000, ...options } = {}) {
    super(roots, {
      interval,
      ...(process.platform === "linux"
        ? { readRss: async () => linuxTreeRss(await roots()) }
        : {}),
      ...options,
    });
    this.path = path;
  }

  async sample() {
    const count = this.samples.length;
    await super.sample();
    if (this.samples.length !== count) {
      try {
        await appendFile(this.path, `${JSON.stringify(this.samples.at(-1))}\n`);
      } catch (error) {
        this.errors.add(`RSS evidence: ${error.message}`);
      }
    }
  }
}

export function fileProgress(text) {
  const matches = [
    ...stripVTControlCharacters(text).matchAll(
      /Indexing files:\s*([\d,]+)\s*\/\s*([\d,]+)/g,
    ),
  ];
  const last = matches.at(-1);
  return last
    ? {
        completed_files: Number(last[1].replaceAll(",", "")),
        total_files: Number(last[2].replaceAll(",", "")),
      }
    : null;
}

/** Stream full logs to disk, retain bounded tails, and terminate the process group. */
export async function measureIndex(command, args, options) {
  const {
    cwd,
    env,
    timeout,
    evidence,
    signal,
    heartbeat = 30_000,
    killGrace = 1500,
    onProgress = async () => {},
  } = options;
  signal?.throwIfAborted();
  const started = performance.now();
  const logs = Object.fromEntries(
    ["stdout", "stderr"].map((name) => [
      name,
      createWriteStream(join(evidence, `index.${name}.log`)),
    ]),
  );
  let child, monitor, timer, ticker, killer;
  let stopped = null;
  let progress = null;
  const tails = { stdout: "", stderr: "" };
  let checkpoint = Promise.resolve();
  const update = (status) => {
    checkpoint = checkpoint.then(async () => {
      const record = {
        status,
        elapsed_ms: performance.now() - started,
        ...progress,
        rss_samples: monitor?.samples.length ?? 0,
        latest_rss_bytes: monitor?.samples.at(-1)?.rss_bytes ?? null,
        observed_at: new Date().toISOString(),
      };
      await writeJson(join(evidence, "index-progress.json"), record);
      await onProgress(record);
      console.log(
        `Index ${status}: ${progress ? `${progress.completed_files}/${progress.total_files} files` : "file count not yet reported"}; ${(record.elapsed_ms / 60_000).toFixed(1)} min; ${record.rss_samples} RSS samples`,
      );
    });
    return checkpoint;
  };
  const kill = (name) => {
    try {
      if (process.platform === "win32") child.kill(name);
      else process.kill(-child.pid, name);
    } catch (error) {
      if (error.code !== "ESRCH") throw error;
    }
  };
  const stop = (reason) => {
    if (stopped) return;
    stopped = reason;
    kill("SIGTERM");
    killer = setTimeout(() => kill("SIGKILL"), killGrace);
  };
  const abort = () => stop("cancelled");
  let result, measurements, processError;
  try {
    child = spawn(command, args, {
      cwd,
      env,
      detached: process.platform !== "win32",
      stdio: ["ignore", "pipe", "pipe"],
    });
    const exited = new Promise((resolve, reject) => {
      child.once("error", reject);
      child.once("close", (code, exitSignal) =>
        resolve({ code, signal: exitSignal }),
      );
      for (const stream of Object.values(logs)) stream.once("error", reject);
    });
    // Register listeners before the first asynchronous checkpoint; fast exits
    // must not be lost while the report is being written.
    void exited.catch(() => {});
    monitor = new DurableMonitor(
      async () => [child.pid],
      join(evidence, "index-rss.jsonl"),
    ).start();
    for (const name of ["stdout", "stderr"]) {
      child[name].pipe(logs[name]);
      child[name].on("data", (part) => {
        tails[name] = (tails[name] + String(part)).slice(-65_536);
        progress = fileProgress(tails[name]) ?? progress;
      });
    }
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
    timer = setTimeout(() => stop("timeout"), timeout);
    ticker = setInterval(() => {
      void update("running").catch(() => stop("evidence_write_failed"));
    }, heartbeat);
    await update("running");
    const exit = await exited;
    result = {
      ...tails,
      ...exit,
      timed_out: stopped === "timeout",
      cancelled: stopped === "cancelled",
      log_scope:
        "bounded 64 KiB tails; complete stdout/stderr in adjacent logs",
      command: [command, ...args],
    };
  } catch (error) {
    processError = error;
    result = {
      ...tails,
      code: null,
      error: error.message,
      command: [command, ...args],
    };
  } finally {
    clearTimeout(timer);
    clearInterval(ticker);
    // A parent may exit before a descendant. Kill remaining group members before
    // cancelling the escalation timer or accepting the result.
    if (child?.pid) kill("SIGKILL");
    clearTimeout(killer);
    signal?.removeEventListener("abort", abort);
    if (monitor) measurements = await monitor.stop();
    for (const stream of Object.values(logs)) stream.end();
    await Promise.all(Object.values(logs).map((stream) => finished(stream)));
    await checkpoint;
  }
  measurements.status =
    !processError && !stopped && result.code === 0 ? "success" : "failed";
  measurements.concurrency = 4;
  if (measurements.status === "success") {
    try {
      measurements.index_bytes = await directoryBytes(join(cwd, ".zvec-grep"));
    } catch (error) {
      measurements.index_bytes = null;
      measurements.index_size_error = error.message;
      measurements.status = "failed";
      stopped = "index_size_unavailable";
    }
  }
  await update(measurements.status);
  await writeJson(join(evidence, "index.json"), { ...result, measurements });
  if (measurements.status !== "success")
    throw Object.assign(
      new Error(`index ${stopped ?? `exit ${result.code}`}`),
      {
        result,
        measurements,
      },
    );
  return { ...result, measurements };
}
