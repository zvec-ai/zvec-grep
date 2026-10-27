import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { lstat, readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { performance } from "node:perf_hooks";
import { promisify } from "node:util";
import { run } from "./io.mjs";
import { statistics } from "../metrics/statistics.mjs";

export const BENCHMARK_CONCURRENCY = 4;
export const MEMORY_SAMPLE_INTERVAL_MS = 100;
const execute = promisify(execFile);

export function indexConcurrencyArguments(help) {
  const flag = help.includes("--index-embedding-concurrency")
    ? "--index-embedding-concurrency"
    : "--embedding-concurrency";
  assert.ok(
    help.includes(flag),
    "candidate cannot fix index embedding concurrency",
  );
  return [flag, String(BENCHMARK_CONCURRENCY)];
}

/** Dispatch in plan order with a fixed in-flight limit; return in plan order. */
export async function mapConcurrent(items, callback) {
  const results = new Array(items.length);
  let next = 0;
  let failure;
  await Promise.all(
    Array.from(
      { length: Math.min(BENCHMARK_CONCURRENCY, items.length) },
      async () => {
        while (!failure && next < items.length) {
          const index = next++;
          try {
            results[index] = await callback(items[index], index);
          } catch (error) {
            failure ??= error;
          }
        }
      },
    ),
  );
  if (failure) throw failure;
  return results;
}

export function processTreeRss(table, roots) {
  const rows = table
    .trim()
    .split("\n")
    .map((line) => {
      const [pid, parent, kib] = line.trim().split(/\s+/).map(Number);
      return { pid, parent, kib };
    })
    .filter((row) => [row.pid, row.parent, row.kib].every(Number.isFinite));
  const included = new Set(
    roots.filter((pid) => Number.isInteger(pid) && pid > 0),
  );
  let size;
  do {
    size = included.size;
    for (const row of rows) if (included.has(row.parent)) included.add(row.pid);
  } while (size !== included.size);
  const selected = rows.filter((row) => included.has(row.pid));
  return selected.length
    ? selected.reduce((sum, row) => sum + row.kib * 1024, 0)
    : null;
}

export async function daemonPid(home) {
  try {
    const record = JSON.parse(
      await readFile(join(home, "daemon/instance.lock"), "utf8"),
    );
    return record.pid;
  } catch (error) {
    if (error.code === "ENOENT") return undefined;
    throw error;
  }
}

/** Sample product RSS, including the detached resident daemon, outside RT timers. */
export class ResourceMonitor {
  constructor(roots, { interval = MEMORY_SAMPLE_INTERVAL_MS, readRss } = {}) {
    this.roots = roots;
    this.interval = interval;
    this.readRss =
      readRss ??
      (async () => {
        const pids = await this.roots();
        const { stdout } = await execute("ps", ["-axo", "pid=,ppid=,rss="], {
          timeout: 5000,
        });
        return processTreeRss(stdout, pids);
      });
    this.samples = [];
    this.errors = new Set();
  }

  start() {
    this.started = performance.now();
    this.active = true;
    const tick = () => {
      this.pending = this.sample().finally(() => {
        if (this.active) this.timer = setTimeout(tick, this.interval);
      });
    };
    tick();
    return this;
  }

  async sample() {
    try {
      const bytes = await this.readRss();
      if (Number.isFinite(bytes) && bytes > 0)
        this.samples.push({
          elapsed_ms: performance.now() - this.started,
          rss_bytes: bytes,
        });
    } catch (error) {
      this.errors.add(error.message);
    }
  }

  async stop() {
    const total_ms = performance.now() - this.started;
    this.active = false;
    clearTimeout(this.timer);
    await this.pending;
    return {
      total_ms,
      memory: {
        status: this.samples.length
          ? this.errors.size
            ? "partial"
            : "available"
          : "unavailable",
        scope:
          "sum of product process-tree RSS, including resident daemon; shared pages may be counted more than once",
        sample_interval_ms: this.interval,
        rss_bytes: statistics(this.samples.map((sample) => sample.rss_bytes)),
        samples: this.samples,
        errors: [...this.errors],
      },
    };
  }
}

export async function directoryBytes(root) {
  let bytes = 0;
  for (const entry of await readdir(root, { withFileTypes: true })) {
    const path = join(root, entry.name);
    if (entry.isDirectory()) bytes += await directoryBytes(path);
    else if (entry.isFile()) bytes += (await lstat(path)).size;
  }
  return bytes;
}

export async function measureIndex(command, args, options) {
  let pid;
  const monitor = new ResourceMonitor(async () => [pid]).start();
  let result;
  let error;
  try {
    result = await run(command, args, {
      ...options,
      onSpawn: (child) => {
        pid = child.pid;
      },
    });
  } catch (failure) {
    error = failure;
    result = failure.result;
  }
  const measured = await monitor.stop();
  measured.status = error ? "failed" : "success";
  measured.concurrency = BENCHMARK_CONCURRENCY;
  if (error) {
    error.measurements = measured;
    throw error;
  }
  try {
    measured.index_bytes = await directoryBytes(
      join(options.cwd, ".zvec-grep"),
    );
  } catch (failure) {
    measured.index_bytes = null;
    measured.index_size_error = failure.message;
  }
  return { ...result, measurements: measured };
}
