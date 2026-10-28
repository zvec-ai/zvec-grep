import assert from "node:assert/strict";
import { join } from "node:path";
import { readJson, fileHash } from "../zg-retrieval/core/io.mjs";
import {
  parseVisibleResponse,
  validateSearchRoute,
} from "../zg-retrieval/engines/zg/parse.mjs";
import { summarizeRepeatedQuality } from "../zg-retrieval/metrics/repetitions.mjs";
import { statistics } from "../zg-retrieval/metrics/statistics.mjs";
import { summarizePilotRows } from "../zg-retrieval/expansion/run.mjs";
import { summarizePerformance } from "../zg-retrieval/reports/performance.mjs";

export const MODES = ["hybrid", "fts", "vector"];

export function relativeChange(node, rust) {
  return Number.isFinite(node) && Number.isFinite(rust) && node !== 0
    ? ((rust - node) / node) * 100
    : null;
}

/** Reparse saved public responses and recompute quality/performance aggregates. */
export async function auditArm(arm, dataset, directory) {
  assert.equal(arm.rows.length, dataset.tasks.length * MODES.length);
  assert.equal(arm.failures.length, 0, "product or preparation failures");
  for (const key of [
    "corpus_before_sha256",
    "corpus_after_sha256",
    "model_before_sha256",
    "model_after_sha256",
  ])
    assert.match(arm[key] ?? "", /^[a-f0-9]{64}$/, `missing ${key}`);
  assert.equal(arm.corpus_before_sha256, arm.corpus_after_sha256);
  assert.equal(arm.model_before_sha256, arm.model_after_sha256);
  const keys = new Set();
  for (const row of arm.rows) {
    const key = `${row.task_id}/${row.mode}`;
    assert.ok(!keys.has(key), "duplicate query/mode evidence");
    keys.add(key);
    const task = dataset.tasks.find((task) => task.id === row.task_id);
    assert.ok(task && MODES.includes(row.mode));
    assert.equal(row.query, task.query);
    assert.deepEqual(row.targets, task.targets);
    assert.equal(row.status, "success");
    assert.equal(row.group_integrity, "verified");
    assert.deepEqual(
      row.calls.map((call) => call.repetition),
      [1, 2, 3, 4, 5],
    );
    for (const call of row.calls) {
      assert.equal(call.status, "success");
      assert.ok(Number.isFinite(call.latency_ms) && call.latency_ms >= 0);
      const path = join(
        directory,
        "evidence/full-corpus/raw",
        `${encodeURIComponent(task.id)}-${row.mode}-${call.repetition}.json`,
      );
      assert.equal(await fileHash(path), call.raw_sha256);
      const raw = await readJson(path);
      assert.equal(raw.request.name, "zvec_grep_search");
      assert.deepEqual(raw.request.arguments, {
        root: arm.corpus_root,
        ...(row.mode === "hybrid"
          ? { query: task.query }
          : { [row.mode]: [task.query] }),
        limit: 10,
        autoUpdate: false,
        freshness: "eventual",
        preferSymbol: false,
      });
      assert.notEqual(raw.response.isError, true);
      const parsed = parseVisibleResponse(raw.response);
      validateSearchRoute(parsed.items, row.mode);
      assert.deepEqual(
        parsed.items.map(({ rank, path, range, matched_by }) => ({
          rank,
          path,
          range,
          matched_by,
        })),
        call.items,
      );
    }
    assert.deepEqual(
      row.quality_mean,
      summarizeRepeatedQuality(row.calls, task.targets),
    );
  }
  const group = arm.performance_groups[0];
  assert.equal(arm.performance_groups.length, 1);
  for (const mode of MODES) {
    const expected = arm.rows
      .filter((row) => row.mode === mode)
      .flatMap((row) => row.calls.map((call) => call.latency_ms));
    assert.deepEqual(group.query[mode].samples_ms, expected);
    assert.deepEqual(group.query[mode].latency_ms, statistics(expected));
    assert.equal(group.query[mode].failed_calls, 0);
  }
  for (const stage of [group.index, ...Object.values(group.query)]) {
    assert.equal(stage.status, "success");
    assert.equal(stage.concurrency, 4);
    assert.ok(Number.isFinite(stage.total_ms) && stage.total_ms > 0);
    assert.equal(stage.memory.status, "available", "RSS sampling incomplete");
    assert.ok(stage.memory.samples.length > 0);
    assert.ok(stage.memory.samples.every((s) => s.rss_bytes > 0));
    assert.deepEqual(
      stage.memory.rss_bytes,
      statistics(stage.memory.samples.map((s) => s.rss_bytes)),
    );
  }
  assert.ok(
    Number.isSafeInteger(group.index.index_bytes) &&
      group.index.index_bytes > 0,
  );
  return {
    calls: arm.rows.reduce((sum, row) => sum + row.calls.length, 0),
    quality: summarizePilotRows(arm.rows, MODES, dataset.tasks.length),
    performance: summarizePerformance(arm.performance_groups),
  };
}

const number = (value, divisor = 1) =>
  Number.isFinite(value) ? (value / divisor).toFixed(2) : "—";
const percent = (a, b) => {
  const change = relativeChange(a, b);
  return change === null
    ? "—"
    : `${change >= 0 ? "+" : ""}${change.toFixed(1)}%`;
};

export function markdownReport(report) {
  const node = report.arms.node,
    rust = report.arms.rust;
  const lines = [
    "# BrowseComp-Plus Retrieval-only",
    "",
    `**${report.status === "success" ? "✅ Complete" : "❌ Failed / incomplete"}**`,
    "",
    `Node: official 0.2.2. Rust: \`${report.rust_commit}\`. Harness: \`${report.harness_commit}\`.`,
    "",
    `${report.dataset?.tasks.length ?? 0} fixed original queries · ${report.dataset?.corpus.count ?? 0} complete corpus documents · hybrid / fts / vector · five calls per query/mode · concurrency limit 4. No answering agent, query rewriting or judge participates.`,
    "",
    "## Document retrieval quality",
    "",
    "Each cell is Node / Rust (relative change). Gold annotations map to one Markdown file per document. Native result ranks, including repeated document chunks, are preserved. These pilot metrics are not official BrowseComp answer accuracy or multi-hop evidence sufficiency.",
    "",
    "| Mode | Doc Hit@1 | Doc Hit@5 | Doc Hit@10 | MRR@10 | nDCG@10 |",
    "| --- | ---: | ---: | ---: | ---: | ---: |",
  ];
  const qualityNumber = (value) =>
    Number.isFinite(value) ? value.toFixed(4) : "—";
  const pair = (a, b) =>
    `${qualityNumber(a)} / ${qualityNumber(b)} (${percent(a, b)})`;
  for (const mode of MODES) {
    const a = node?.quality?.find((row) => row.mode === mode)?.metrics;
    const b = rust?.quality?.find((row) => row.mode === mode)?.metrics;
    lines.push(
      `| ${mode} | ${["file_hit_at_1", "file_hit_at_5", "file_hit_at_10", "file_mrr_at_10", "ndcg_at_10"].map((key) => pair(a?.[key], b?.[key])).join(" | ")} |`,
    );
  }
  lines.push(
    "",
    "## Index & Query performance",
    "",
    "Relative change = (Rust − Node) / Node × 100%. Negative means lower. Missing values and zero denominators are —.",
    "",
    "| Stage | Metric | Unit | Node 0.2.2 | Rust | Relative change |",
    "| --- | --- | --- | ---: | ---: | ---: |",
  );
  for (const mode of ["index", ...MODES]) {
    const a =
      mode === "index"
        ? node?.performance?.index
        : node?.performance?.query[mode];
    const b =
      mode === "index"
        ? rust?.performance?.index
        : rust?.performance?.query[mode];
    const stage = mode === "index" ? "Index" : `Query ${mode}`;
    const add = (metric, unit, left, right, divisor = 1) =>
      lines.push(
        `| ${stage} | ${metric} | ${unit} | ${number(left, divisor)} | ${number(right, divisor)} | ${percent(left, right)} |`,
      );
    for (const key of ["total", "mean", "p95", "p99"])
      add(`phase time ${key}`, "s", a?.total_ms[key], b?.total_ms[key], 1000);
    if (mode !== "index")
      for (const key of ["mean", "p50", "p95", "p99"])
        add(`request RT ${key}`, "ms", a?.latency_ms[key], b?.latency_ms[key]);
    for (const key of ["mean", "p95", "p99", "max"])
      add(
        `sampled RSS ${key}`,
        "MiB",
        a?.memory_rss_bytes[key],
        b?.memory_rss_bytes[key],
        1024 ** 2,
      );
    if (mode === "index") {
      add(
        "generated index total bytes",
        "bytes",
        a?.index_bytes.total,
        b?.index_bytes.total,
      );
      for (const key of ["total", "mean", "p95", "p99"])
        add(
          `generated index ${key}`,
          "MiB",
          a?.index_bytes[key],
          b?.index_bytes[key],
          1024 ** 2,
        );
    }
  }
  lines.push(
    "",
    "## Coverage",
    "",
    "| Runtime | Calls | Index phases | Query phases per mode | RSS samples: index / hybrid / fts / vector |",
    "| --- | ---: | ---: | ---: | --- |",
  );
  for (const [name, arm] of Object.entries(report.arms)) {
    const p = arm.performance;
    lines.push(
      `| ${name} | ${arm.calls ?? 0}/150 | ${p?.index.measured ?? 0}/1 | ${MODES.map((m) => p?.query[m].measured ?? 0).join(" / ")} | ${[p?.index, ...MODES.map((m) => p?.query[m])].map((stage) => stage?.memory_rss_bytes.count ?? 0).join(" / ")} |`,
    );
  }
  lines.push(
    "",
    "Each runtime builds one fresh complete corpus index, so Index and per-mode phase-time mean/p95/p99 each describe one observation. They do not estimate run-to-run variation. RT has 50 calls per mode. p50 is the median; p95/p99 use nearest rank over pooled raw samples.",
    "",
    "Model downloads are warmed separately before measurement. Index includes CLI startup, model loading and optimization. Query RT includes first-call/session/cache effects; group totals include capture. Both products run sequentially on the same host, with fixed hybrid → fts → vector order and a shared pinned model cache. This is one observed paired run, without a controlled causal speed claim.",
    "",
    "RSS samples sum product process trees and resident daemons, excluding the driver; shared pages may be double-counted and 100 ms target sampling can miss spikes. Index bytes include regular .zvec-grep files after building, excluding model cache and symlinks. Index embedding batches and public queries have a limit of four; storage commits retain native scheduling. Per-file write RT is excluded.",
  );
  if (report.errors.length)
    lines.push(
      "",
      "## Failures",
      "",
      ...report.errors.map(
        (error) => `- ${String(error).replace(/[\r\n|]/g, " ")}`,
      ),
    );
  if (report.run_url)
    lines.push("", `[CI run and raw artifacts](${report.run_url})`);
  return `${lines.join("\n")}\n`;
}
