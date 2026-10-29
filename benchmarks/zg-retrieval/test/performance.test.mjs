import assert from "node:assert/strict";
import test from "node:test";
import { mkdtemp, mkdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { statistics } from "../metrics/statistics.mjs";
import {
  mapConcurrent,
  ResourceMonitor,
  processTreeRss,
  measureIndex,
  indexConcurrencyArguments,
  daemonPid,
} from "../core/performance.mjs";
import {
  summarizePerformance,
  markdownPerformance,
} from "../reports/performance.mjs";

test("percentiles use nearest rank and preserve empty/zero populations", () => {
  assert.deepEqual(statistics([]), {
    count: 0,
    mean: null,
    p50: null,
    p95: null,
    p99: null,
    max: null,
    total: null,
  });
  const samples = Array.from(
    { length: 100 },
    (_, index) => index + 1,
  ).reverse();
  assert.deepEqual(statistics(samples), {
    count: 100,
    mean: 50.5,
    p50: 50.5,
    p95: 95,
    p99: 99,
    max: 100,
    total: 5050,
  });
  assert.equal(statistics([0]).p99, 0);
  assert.equal(samples[0], 100);
});

test("new and legacy Rust index flags both cap concurrency at four", () => {
  assert.deepEqual(
    indexConcurrencyArguments(
      "  --embedding-concurrency <n>        Positive embedding concurrency (index only)\n",
    ),
    ["--embedding-concurrency", "4"],
  );
  assert.deepEqual(
    indexConcurrencyArguments("--index-embedding-concurrency <n>"),
    ["--index-embedding-concurrency", "4"],
  );
});

test("the current index flag takes precedence when both names are advertised", () => {
  assert.deepEqual(
    indexConcurrencyArguments(
      "--index-embedding-concurrency <n>\n--embedding-concurrency <n>",
    ),
    ["--embedding-concurrency", "4"],
  );
});

test("missing or lookalike index flags cannot silently use default concurrency", () => {
  for (const help of [
    "",
    "unsupported",
    "--embedding-concurrency-extra <n>",
    "--index-embedding-concurrency-extra <n>",
  ]) {
    assert.throws(() => indexConcurrencyArguments(help), /cannot fix/);
  }
});

test("four requests can overlap, finish out of order and retain plan order", async () => {
  let active = 0;
  let peak = 0;
  const releases = [];
  const dispatched = [];
  const pending = mapConcurrent([0, 1, 2, 3, 4], async (value) => {
    dispatched.push(value);
    peak = Math.max(peak, ++active);
    await new Promise((resolve) => {
      releases[value] = resolve;
    });
    active--;
    return value;
  });
  assert.deepEqual(dispatched, [0, 1, 2, 3]);
  releases[3]();
  await new Promise(setImmediate);
  assert.deepEqual(dispatched, [0, 1, 2, 3, 4]);
  for (const value of [4, 2, 1, 0]) releases[value]();
  assert.deepEqual(await pending, [0, 1, 2, 3, 4]);
  assert.equal(peak, 4);
  assert.deepEqual(await mapConcurrent([], () => assert.fail()), []);
});

test("a worker failure drains in-flight requests before cleanup and stops dispatch", async () => {
  let finish;
  const started = [];
  const pending = mapConcurrent([0, 1, 2, 3, 4], async (value) => {
    started.push(value);
    if (value === 0) throw new Error("capture failed");
    if (value === 1)
      await new Promise((resolve) => {
        finish = resolve;
      });
  });
  let settled = false;
  pending.catch(() => {
    settled = true;
  });
  await new Promise(setImmediate);
  assert.equal(settled, false);
  finish();
  await assert.rejects(pending, /capture failed/);
  assert.ok(!started.includes(4));
});

test("RSS sums the product tree and detached daemon once, excluding the driver", () => {
  const table =
    "100 1 500\n200 100 10\n201 200 20\n300 1 30\n301 300 40\n400 100 999\n";
  assert.equal(processTreeRss(table, [200, 201, 300, undefined]), 100 * 1024);
  assert.equal(processTreeRss(table, [999]), null);
});

test("memory sampling records unavailability instead of a false zero", async () => {
  const failed = new ResourceMonitor(() => [], {
    readRss: async () => {
      throw new Error("ps unavailable");
    },
  }).start();
  const missing = await failed.stop();
  assert.equal(missing.memory.status, "unavailable");
  assert.equal(missing.memory.rss_bytes.mean, null);
  assert.deepEqual(missing.memory.errors, ["ps unavailable"]);
  const monitor = new ResourceMonitor(() => [], {
    readRss: async () => 4096,
  }).start();
  const measured = await monitor.stop();
  assert.equal(measured.memory.rss_bytes.p99, 4096);
  assert.equal(measured.memory.samples.length, 1);
});

test("index monitoring captures a live child, counts index bytes and retains failure timing", async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "zg-index-performance-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  await mkdir(join(directory, ".zvec-grep/nested"), { recursive: true });
  await writeFile(join(directory, ".zvec-grep/nested/index"), "1234567");
  await writeFile(join(directory, "model-cache"), "excluded");
  await symlink(
    join(directory, "model-cache"),
    join(directory, ".zvec-grep/link"),
  );
  await mkdir(join(directory, "daemon"));
  assert.equal(await daemonPid(directory), undefined);
  await writeFile(
    join(directory, "daemon/instance.lock"),
    JSON.stringify({ pid: process.pid }),
  );
  assert.equal(await daemonPid(directory), process.pid);
  const result = await measureIndex(
    process.execPath,
    ["-e", "setTimeout(() => {}, 350)"],
    { cwd: directory },
  );
  assert.equal(result.measurements.index_bytes, 7);
  assert.equal(Object.hasOwn(result.measurements, "write"), false);
  assert.ok(result.measurements.total_ms >= 350);
  if (process.platform !== "win32") {
    assert.equal(result.measurements.memory.status, "available");
    assert.ok(result.measurements.memory.rss_bytes.mean > 0);
  }
  await assert.rejects(
    measureIndex(
      process.execPath,
      ["-e", "setTimeout(() => process.exit(2), 120)"],
      { cwd: directory },
    ),
    (error) => {
      assert.equal(error.measurements.status, "failed");
      assert.ok(error.measurements.total_ms > 0);
      return true;
    },
  );
  await assert.rejects(
    measureIndex(process.execPath, ["-e", "setInterval(() => {}, 1000)"], {
      cwd: directory,
      timeout: 30,
    }),
    (error) => {
      assert.equal(error.result.timed_out, true);
      assert.equal(error.measurements.status, "failed");
      return true;
    },
  );
  await assert.rejects(
    measureIndex(join(directory, "missing-command"), [], { cwd: directory }),
    (error) => {
      assert.equal(error.code, "ENOENT");
      assert.equal(error.measurements.status, "failed");
      return true;
    },
  );
});

test("final tables pool raw RT/RSS samples and report missing phase coverage", () => {
  const index = (total, values) => ({
    status: "success",
    concurrency: 4,
    total_ms: total,
    index_bytes: 1024,
    memory: {
      status: "available",
      samples: values.map((value) => ({ rss_bytes: value * 1024 ** 2 })),
    },
  });
  const result = summarizePerformance([
    {
      index: index(1000, [1, 2, 3]),
      query: { hybrid: { samples_ms: [1, 2, 3] } },
    },
    { index: index(2000, [100]), query: { hybrid: { samples_ms: [100] } } },
    {},
  ]);
  assert.equal(result.query.hybrid.latency_ms.mean, 26.5);
  assert.equal(result.query.hybrid.latency_ms.p95, 100);
  assert.equal(result.index.total_ms.total, 3000);
  assert.equal(result.index.index_bytes.total, 2048);
  assert.equal(result.index.planned, 3);
  assert.equal(Object.hasOwn(result.index, "latency_ms"), false);
  assert.equal(Object.hasOwn(result.index, "write_measured"), false);
  assert.equal(result.query.fts.total_ms.mean, null);
  const text = markdownPerformance(result);
  assert.match(text, /Index \| 2\/3/);
  assert.match(text, /26.50 \/ 2.50 \/ 100.00 \/ 100.00/);
  assert.match(text, /RSS mean \/ p95 \/ p99/);
  assert.match(text, /Generated index size/);
  assert.doesNotMatch(text, /Index RT|index_write|per-file diagnostics/);
});
