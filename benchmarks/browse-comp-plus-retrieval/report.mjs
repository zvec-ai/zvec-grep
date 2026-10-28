import assert from "node:assert/strict";
import { join, resolve } from "node:path";
import { writeFile, mkdir } from "node:fs/promises";
import { parseArgs } from "node:util";
import { pathToFileURL } from "node:url";
import { readJson, fileHash, writeJson } from "../zg-retrieval/core/io.mjs";
import {
  parseVisibleResponse,
  validateSearchRoute,
} from "../zg-retrieval/engines/zg/parse.mjs";
import { summarizeRepeatedQuality } from "../zg-retrieval/metrics/repetitions.mjs";
import { statistics } from "../zg-retrieval/metrics/statistics.mjs";
import { summarizePilotRows } from "../zg-retrieval/expansion/run.mjs";
import { summarizePerformance } from "../zg-retrieval/reports/performance.mjs";

export const MODES = ["hybrid", "fts", "vector"];

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
  for (const stage of [group.index, ...Object.values(group.query)].filter(
    Boolean,
  )) {
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
  if (group.index)
    assert.ok(
      Number.isSafeInteger(group.index.index_bytes) &&
        group.index.index_bytes > 0,
    );
  else
    assert.match(
      arm.index_artifact?.key ?? "",
      /^[a-f0-9]{64}$/,
      "missing reusable index identity",
    );
  return {
    calls: arm.rows.reduce((sum, row) => sum + row.calls.length, 0),
    quality: summarizePilotRows(arm.rows, MODES, dataset.tasks.length),
    performance: summarizePerformance(arm.performance_groups),
  };
}

export function relativeChange(base, head) {
  return Number.isFinite(base) && Number.isFinite(head) && base !== 0
    ? ((head - base) / base) * 100
    : null;
}
const number = (value, divisor = 1) =>
  Number.isFinite(value) ? (value / divisor).toFixed(2) : "—";
const delta = (a, b) => {
  const p = relativeChange(a, b);
  return p === null ? "—" : `${p >= 0 ? "+" : ""}${p.toFixed(1)}%`;
};
const pair = (a, b, divisor = 1) =>
  `${number(a, divisor)} → ${number(b, divisor)} (${delta(a, b)})`;

/** Numeric-only projection shared by Summary and the privileged PR publisher. */
export function validateComparison(report, request) {
  assert.equal(report.schema_version, 1);
  assert.equal(report.kind, "browsecomp-doc-comparison");
  assert.equal(report.harness_commit, request.harness_commit);
  assert.equal(
    report.repository_url,
    `https://github.com/${request.repository}`,
  );
  assert.equal(
    report.run_url,
    `https://github.com/${request.repository}/actions/runs/${request.run_id}`,
  );
  assert.equal(report.document_count, 100195);
  assert.equal(
    report.model_sha256,
    "217e3074d135c2055eed9d3b3c43a3158a904460e4e6ca4b3d492a7ca4f93771",
  );
  for (const name of ["baseline", "candidate"]) {
    const arm = report[name];
    assert.equal(arm.commit, request[`${name}_sha`]);
    assert.match(arm.commit, /^[a-f0-9]{40}$/);
    assert.equal(arm.status, "success");
    assert.equal(arm.calls, 150);
    assert.match(arm.index_artifact.key, /^[a-f0-9]{64}$/);
    assert.match(arm.index_artifact.builder_commit, /^[a-f0-9]{40}$/);
    assert.ok(
      Number.isSafeInteger(arm.index_artifact.artifact_id) &&
        arm.index_artifact.artifact_id > 0,
    );
    assert.ok(
      Number.isSafeInteger(arm.index_artifact.run_id) &&
        arm.index_artifact.run_id > 0,
    );
    for (const mode of MODES) {
      const stage = arm.performance.query[mode];
      assert.equal(stage.latency_ms.count, 50);
      assert.equal(stage.failed, 0);
      assert.equal(stage.measured, 1);
      assert.equal(stage.memory_measured, 1);
      assert.deepEqual(stage.concurrency, [4]);
      assert.ok(stage.memory_rss_bytes.count > 0);
      for (const [stats, keys] of [
        [stage.latency_ms, ["mean", "p50", "p95", "p99"]],
        [stage.total_ms, ["total", "mean", "p95", "p99"]],
        [stage.memory_rss_bytes, ["mean", "p95", "p99", "max"]],
      ])
        for (const key of keys)
          assert.ok(Number.isFinite(stats[key]) && stats[key] >= 0);
      const quality = arm.quality.find((row) => row.mode === mode);
      assert.equal(quality.completed, 10);
      for (const key of [
        "file_hit_at_1",
        "file_hit_at_5",
        "file_hit_at_10",
        "file_mrr_at_10",
        "ndcg_at_10",
      ]) {
        const value = quality.metrics[key];
        assert.ok(Number.isFinite(value) && value >= 0 && value <= 1);
      }
    }
    const index = arm.historical_index.performance.index;
    assert.equal(index.measured, 1);
    assert.equal(index.failed, 0);
    assert.equal(index.memory_measured, 1);
    assert.deepEqual(index.concurrency, [4]);
    for (const [stats, keys] of [
      [index.total_ms, ["total", "mean", "p95", "p99"]],
      [index.memory_rss_bytes, ["mean", "p95", "p99", "max", "count"]],
      [index.index_bytes, ["total", "mean", "p95", "p99", "count"]],
    ])
      for (const key of keys)
        assert.ok(Number.isFinite(stats[key]) && stats[key] > 0);
    assert.ok(index.index_bytes.total > 0);
  }
  assert.equal(report.status, "success");
  return report;
}

export function markdownReport(report) {
  const a = report.baseline,
    b = report.candidate;
  if (report.status !== "success" || !a || !b)
    return "# BrowseComp+ Retrieval-only · failed / incomplete\n\nNo passing full-corpus comparison is available. Missing measurements are unavailable, never zero. See the run artifacts for partial evidence.\n";
  const shared = a.index_artifact.key === b.index_artifact.key;
  const lines = [
    "# BrowseComp+ Retrieval-only",
    "",
    "**✅ Complete: PR base Rust → PR head Rust**",
    "",
    `Base: \`${a.commit}\`; head: \`${b.commit}\`; harness: \`${report.harness_commit}\`.`,
    "",
    "100,195 documents · ten fixed original questions · hybrid / fts / vector · five calls per query/mode · 150 calls per version · CPU · concurrency limit 4.",
    "",
    "## Index",
    "",
    "Index data below are **historical Artifact-builder measurements**, not fresh Index timings from this Query run. Each index has one full-build observation; its mean/p95/p99 do not estimate run-to-run variation.",
    "",
    "| Version | Index Artifact | Builder SHA | Original build (s) | Index bytes |",
    "| --- | --- | --- | ---: | ---: |",
  ];
  for (const [name, arm] of [
    ["Base", a],
    ["Head", b],
  ]) {
    const index = arm.historical_index.performance.index;
    lines.push(
      `| ${name} | [${arm.index_artifact.key.slice(0, 12)}](${report.repository_url}/actions/runs/${arm.index_artifact.run_id}/artifacts/${arm.index_artifact.artifact_id}) | \`${arm.index_artifact.builder_commit}\` | ${number(index.total_ms.total, 1000)} | ${number(index.index_bytes.total)} |`,
    );
  }
  lines.push(
    "",
    shared
      ? "Both versions reuse the **same sealed full-corpus index**. Only a changed underlying storage identity requires a new index."
      : "The underlying storage identities differ; each version uses its own compatible sealed full-corpus index.",
    "",
    "<details><summary>Historical Index time, RSS, size and coverage</summary>",
    "",
    "| Metric | Base | Head |",
    "| --- | ---: | ---: |",
  );
  for (const [label, field, keys, divisor] of [
    ["Index time (s)", "total_ms", ["total", "mean", "p95", "p99"], 1000],
    [
      "Index RSS (MiB)",
      "memory_rss_bytes",
      ["mean", "p95", "p99", "max", "count"],
      1024 ** 2,
    ],
    [
      "Index size (MiB)",
      "index_bytes",
      ["total", "mean", "p95", "p99", "count"],
      1024 ** 2,
    ],
  ])
    for (const key of keys)
      lines.push(
        `| ${label} ${key} | ${number(a.historical_index.performance.index[field][key], key === "count" ? 1 : divisor)} | ${number(b.historical_index.performance.index[field][key], key === "count" ? 1 : divisor)} |`,
      );
  lines.push(
    "",
    "Builder hardware and all raw Index samples are retained with the Artifact. Builds from different machines or dates are not controlled speed comparisons.",
    "",
    "</details>",
    "",
    "## Query",
    "",
    "Cells are base → head (relative change). Δ = (head − base) / base × 100%; a zero denominator or missing value is —.",
    "",
    "| Mode | Mean RT (ms) | p95 RT (ms) | Group total (s) | Sampled peak RSS (MiB) |",
    "| --- | ---: | ---: | ---: | ---: |",
  );
  for (const mode of MODES) {
    const x = a.performance.query[mode],
      y = b.performance.query[mode];
    lines.push(
      `| ${mode} | ${pair(x.latency_ms.mean, y.latency_ms.mean)} | ${pair(x.latency_ms.p95, y.latency_ms.p95)} | ${pair(x.total_ms.total, y.total_ms.total, 1000)} | ${pair(x.memory_rss_bytes.max, y.memory_rss_bytes.max, 1024 ** 2)} |`,
    );
  }
  lines.push(
    "",
    "<details><summary>All Query percentiles, totals and sample coverage</summary>",
    "",
    "| Mode | Metric | Base → head (Δ) |",
    "| --- | --- | ---: |",
  );
  for (const mode of MODES) {
    const x = a.performance.query[mode],
      y = b.performance.query[mode];
    for (const [label, field, keys, divisor] of [
      ["RT (ms)", "latency_ms", ["mean", "p50", "p95", "p99", "count"], 1],
      ["Phase (s)", "total_ms", ["total", "mean", "p95", "p99", "count"], 1000],
      [
        "RSS (MiB)",
        "memory_rss_bytes",
        ["mean", "p95", "p99", "max", "count"],
        1024 ** 2,
      ],
    ])
      for (const key of keys)
        lines.push(
          `| ${mode} | ${label} ${key} | ${pair(x[field][key], y[field][key], key === "count" ? 1 : divisor)} |`,
        );
  }
  lines.push(
    "",
    "</details>",
    "",
    "## Retrieval quality",
    "",
    "| Mode | Doc Hit@1 | Doc Hit@5 | Doc Hit@10 | MRR@10 | nDCG@10 |",
    "| --- | ---: | ---: | ---: | ---: | ---: |",
  );
  for (const mode of MODES) {
    const x = a.quality.find((row) => row.mode === mode).metrics,
      y = b.quality.find((row) => row.mode === mode).metrics;
    lines.push(
      `| ${mode} | ${["file_hit_at_1", "file_hit_at_5", "file_hit_at_10", "file_mrr_at_10", "ndcg_at_10"].map((key) => `${x[key].toFixed(4)} → ${y[key].toFixed(4)} (${delta(x[key], y[key])})`).join(" | ")} |`,
    );
  }
  lines.push(
    "",
    "Original gold/evidence and native chunk ranks are preserved. Scores measure document localization, not BrowseComp answer accuracy. Repeated calls are not independent questions.",
    "",
    "Both Query versions execute sequentially on the same Linux host with identical model bytes. First-call/model/page-cache effects remain. RT excludes response parsing/capture; group totals include it. RSS sums product process trees and resident daemons at a 1000 ms target interval; shared pages may be counted twice and short peaks may be missed. No per-file write RT is invented.",
    "",
    `[Run, Summary and raw evidence](${report.run_url})`,
  );
  return lines.join("\n") + "\n";
}

export async function combine({ baseline, candidate, request, output }) {
  const comparison = {
    schema_version: 1,
    kind: "browsecomp-doc-comparison",
    status: "failed",
    harness_commit: request.harness_commit,
    document_count: 100195,
    repository_url: `https://github.com/${request.repository}`,
    run_url: `https://github.com/${request.repository}/actions/runs/${request.run_id}`,
    errors: [],
  };
  try {
    for (const [name, directory] of [
      ["baseline", baseline],
      ["candidate", candidate],
    ]) {
      const report = await readJson(join(directory, "report.json"));
      assert.equal(report.status, "success");
      assert.equal(report.commit, request[`${name}_sha`]);
      assert.equal(report.harness_commit, request.harness_commit);
      const audited = await auditArm(report.arm, report.dataset, directory);
      assert.equal(audited.calls, 150);
      assert.equal(report.document_count, 100195);
      comparison[name] = {
        commit: report.commit,
        status: report.status,
        package: report.package,
        index_artifact: report.index_artifact,
        historical_index: report.historical_index,
        ...audited,
      };
      if (comparison.model_sha256)
        assert.equal(comparison.model_sha256, report.model_artifacts.sha256);
      comparison.model_sha256 = report.model_artifacts.sha256;
      if (comparison.corpus_sha256)
        assert.equal(comparison.corpus_sha256, report.corpus_sha256);
      comparison.corpus_sha256 = report.corpus_sha256;
    }
    comparison.status = "success";
    validateComparison(comparison, request);
  } catch (error) {
    comparison.status = "failed";
    comparison.errors.push(error.message);
    process.exitCode = 1;
  }
  await mkdir(output, { recursive: true });
  await writeJson(join(output, "summary.json"), comparison);
  await writeFile(join(output, "summary.md"), markdownReport(comparison));
  return comparison;
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  const { values } = parseArgs({
    options: Object.fromEntries(
      ["baseline", "candidate", "request", "output"].map((key) => [
        key,
        { type: "string" },
      ]),
    ),
  });
  await combine({
    baseline: resolve(values.baseline),
    candidate: resolve(values.candidate),
    request: await readJson(values.request),
    output: resolve(values.output),
  });
}
