import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { summarizeFileRetrieval } from "../metrics/files.mjs";
import { loadSuite, readJson, writeJson } from "../core/lib.mjs";
import { summarizeMeasurements } from "../metrics/measurements.mjs";
import { summarizeNdcg } from "../metrics/summary.mjs";
import { validateZgReport, ZG_MODES } from "./validation.mjs";
import { markdownPerformance, summarizePerformance } from "./performance.mjs";

export const QUALITY_METRICS = Object.freeze([
  "file_hit_at_1",
  "file_hit_at_5",
  "file_hit_at_10",
  "file_mrr_at_10",
  "ndcg_at_10",
]);

function resultRow(label, rows, expectedQuestions, expectedRepositories) {
  const file = summarizeFileRetrieval(rows);
  const ndcg = summarizeNdcg(rows);
  return {
    label,
    status:
      file.scored_tasks < expectedQuestions
        ? "partial"
        : rows.some((row) =>
              row.measurement_observations.some(
                (sample) => sample.execution_status === "product_error",
              ),
            )
          ? "product_error"
          : "success",
    questions: file.scored_tasks,
    expected_questions: expectedQuestions,
    repositories: ndcg.repository_count,
    expected_repositories: expectedRepositories,
    metrics: file.scored_tasks
      ? {
          file_hit_at_1: file.hit_at_1,
          file_hit_at_5: file.hit_at_5,
          file_hit_at_10: file.hit_at_10,
          file_mrr_at_10: file.mrr_at_10,
          ndcg_at_10: ndcg.repository_macro.ndcg_at_10,
        }
      : null,
    ranking_repeatable_cases: rows.filter(
      (row) => row.ranking_repeatable === true,
    ).length,
    measurements: rows.length
      ? summarizeMeasurements(
          rows.flatMap((row) => row.measurement_observations),
        )
      : null,
  };
}

const unavailableRow = (
  label,
  status,
  expectedQuestions,
  expectedRepositories,
) => ({
  label,
  status,
  questions: 0,
  expected_questions: expectedQuestions,
  repositories: 0,
  expected_repositories: expectedRepositories,
  metrics: null,
  ranking_repeatable_cases: 0,
  measurements: null,
});

function failedTasks(checked) {
  const failures = new Map();
  const add = (row, kind, reason) => {
    const key = JSON.stringify([row.task_id, kind, reason]);
    if (!failures.has(key))
      failures.set(key, {
        task_id: row.task_id,
        repository: row.repository,
        modes: [],
        kind,
        reason,
      });
    failures.get(key).modes.push(row.mode);
  };
  for (const row of checked.invalidRows)
    add(row, "invalid_evidence", row.invalid_reason);
  for (const row of checked.rows.values()) {
    const count = row.measurement_observations.filter(
      (sample) => sample.execution_status === "product_error",
    ).length;
    if (count) add(row, "product_error", `${count}/5 MCP calls failed`);
  }
  return [...failures.values()]
    .map((entry) => ({
      ...entry,
      modes: ZG_MODES.filter((mode) => entry.modes.includes(mode)),
    }))
    .sort((a, b) => a.task_id.localeCompare(b.task_id));
}

/** Fixed ZG arms; invalid evidence never becomes a zero score. */
export async function buildCiSummary({
  zg = null,
  jobResults = {},
  runUrl = null,
  harnessCommit = null,
  candidateCommit = null,
  candidateRef = null,
} = {}) {
  const suite = await loadSuite();
  const rows = [],
    errors = [];
  let failures = [];
  let tasks = [];
  try {
    assert.ok(zg, "missing ZG report");
    const checked = validateZgReport(zg, "ZG", {
      suite,
      allowPartial: true,
    });
    assert.equal(
      zg.scope,
      "full-20-original-queries",
      "CI requires all 20 questions",
    );
    assert.deepEqual(
      checked.ids,
      suite.lock.tasks.map((task) => task.task_id).sort(),
      "CI task selection differs from frozen suite",
    );
    for (const mode of ZG_MODES)
      rows.push(
        resultRow(
          `zg-${mode}`,
          [...checked.rows.values()].filter((row) => row.mode === mode),
          suite.lock.tasks.length,
          suite.lock.repositories.length,
        ),
      );
    failures = failedTasks(checked);
    tasks = [...checked.rows.values(), ...checked.invalidRows]
      .map((row) => {
        const measurements =
          row.status === "harness_invalid"
            ? null
            : summarizeMeasurements(row.measurement_observations);
        const productErrors = row.measurement_observations.filter(
          (sample) => sample.execution_status === "product_error",
        ).length;
        return {
          task_id: row.task_id,
          repository: row.repository,
          mode: row.mode,
          status:
            row.status === "harness_invalid"
              ? "invalid_evidence"
              : productErrors
                ? "product_error"
                : "scored",
          reason:
            row.status === "harness_invalid"
              ? row.invalid_reason
              : productErrors
                ? `${productErrors}/5 MCP calls failed`
                : null,
          file: row.quality_mean?.file ?? null,
          ndcg_at_10: row.quality_mean?.ndcg_at_10 ?? null,
          ranking_repeatable: row.ranking_repeatable,
          hit_at_10_calls: row.quality_mean?.hit_at_10_calls ?? null,
          measurements,
        };
      })
      .sort(
        (a, b) =>
          a.task_id.localeCompare(b.task_id) ||
          ZG_MODES.indexOf(a.mode) - ZG_MODES.indexOf(b.mode),
      );
    if (checked.invalidRows.length)
      errors.push(
        `ZG: ${checked.invalidRows.length} task/mode observations have invalid evidence; partial metrics exclude them`,
      );
    if (zg.product_error_calls)
      errors.push(`ZG: ${zg.product_error_calls} product calls failed`);
  } catch (error) {
    errors.push(`ZG: ${error.message}`);
    rows.length = 0;
    rows.push(
      ...ZG_MODES.map((mode) =>
        unavailableRow(
          `zg-${mode}`,
          "invalid",
          suite.lock.tasks.length,
          suite.lock.repositories.length,
        ),
      ),
    );
  }
  const requiredJobs = [
    "authorize",
    "quality-contract",
    "package-candidate",
    "sweqa",
  ];
  if (Object.keys(jobResults).length)
    for (const job of requiredJobs)
      if (jobResults[job]?.result !== "success")
        errors.push(`${job}: ${jobResults[job]?.result ?? "missing job"}`);
  return {
    schema_version: 5,
    runtime: zg?.runtime ?? "rust-native",
    status: errors.length ? "failed" : "success",
    model: suite.protocol.model,
    preview: "mcp-default",
    quality_metrics: QUALITY_METRICS,
    run_url: runUrl,
    harness_commit: harnessCommit,
    candidate: {
      ref: candidateRef,
      commit: candidateCommit,
    },
    rows,
    performance: Array.isArray(zg?.repositories)
      ? summarizePerformance(
          zg.repositories.map((manifest) => manifest?.performance ?? {}),
        )
      : null,
    tasks,
    failed_tasks: failures,
    errors,
  };
}

const cell = (value) =>
  String(value).replaceAll("|", "\\|").replaceAll(/\r?\n/g, " ");
const number = (value, places = 4) =>
  Number.isFinite(value) ? value.toFixed(places) : "—";

export function markdownCiSummary(result) {
  const status =
    result.status === "success" ? "✅ Complete" : "❌ Failed / incomplete";
  const lines = [
    "# SWE-QA20 Retrieval-only results",
    "",
    `**${status}** · 20 original questions · 11 pinned repositories · ZG hybrid / fts / vector · Rust MCP default presentation`,
    "",
    "| Arm | Status | Coverage | Stable Top 10 | File Hit@1 | File Hit@5 | File Hit@10 | File MRR@10 | nDCG@10 | Mean output (KiB) | Avg RT (ms) | P50 RT (ms) |",
    "| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
  ];
  for (const row of result.rows) {
    const state = {
      success: "✅ Valid",
      product_error: "⚠️ Product-call failure",
      partial: "⚠️ Partial",
      invalid: "❌ No valid report",
    }[row.status];
    const hits = [1, 5, 10].map((cutoff) => {
      const value = row.metrics?.[`file_hit_at_${cutoff}`];
      return Number.isFinite(value) ? `${(value * 100).toFixed(1)}%` : "—";
    });
    lines.push(
      `| ${cell(row.label)} | ${state} | ${row.questions}/${row.expected_questions} questions; ${row.repositories}/${row.expected_repositories} repositories | ${row.ranking_repeatable_cases}/${row.questions} | ${hits.join(" | ")} | ${number(row.metrics?.file_mrr_at_10)} | ${number(row.metrics?.ndcg_at_10)} | ${number(row.measurements?.output_bytes_mean == null ? null : row.measurements.output_bytes_mean / 1024, 2)} | ${number(row.measurements?.latency_ms_mean, 2)} | ${number(row.measurements?.latency_ms_p50, 2)} |`,
    );
  }
  lines.push(
    "",
    "All three arms use the Rust public MCP default presentation and five calls per question. No preview override is sent. Each question/mode averages its five call-level quality scores; Hit/MRR then average questions and nDCG@10 averages repositories. Stable Top 10 means the five ordered public result locations match. Invalid evidence is excluded, never converted to a zero. Product errors with valid evidence retain zero quality credit. Partial scores are diagnostic and are not directly comparable with complete 20-question scores. Finding a file does not establish sufficient answer evidence.",
    "",
    "Output is the mean UTF-8 byte count of successful fifth-call responses (1 KiB = 1024 bytes, not model tokens). Avg RT and P50 RT use successful valid MCP search calls, excluding indexing. Failed and invalid calls are excluded from these measurements. A complete successful arm has 20 output samples and 100 latency samples. Index loading and fixed mode order affect latency; these are observations of this run, not a controlled speed comparison.",
    "",
    ...result.rows
      .filter((row) => row.measurements)
      .map(
        (row) =>
          `- ${cell(row.label)}: ${row.measurements.output_sample_count} output samples; ${row.measurements.latency_sample_count} latency samples.`,
      ),
  );
  lines.push("", markdownPerformance(result.performance));
  if (result.tasks?.length) {
    lines.push(
      "",
      "<details>",
      `<summary>Per-question results (${result.tasks.length} question/mode rows)</summary>`,
      "",
      "| Question | Repository | Arm | Status | Stable Top 10 | Hit@10 calls | Hit@1 | Hit@5 | Hit@10 | RR@10 | nDCG@10 | Output (KiB) | Avg RT (ms) | P50 RT (ms) |",
      "| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    );
    for (const task of result.tasks) {
      const status =
        task.status === "scored"
          ? "✅ Scored"
          : task.status === "product_error"
            ? `⚠️ ${cell(task.reason)}`
            : `❌ ${cell(task.reason)}`;
      lines.push(
        `| ${cell(task.task_id)} | ${cell(task.repository)} | zg-${task.mode} | ${status} | ${task.ranking_repeatable == null ? "—" : task.ranking_repeatable ? "Yes" : "No"} | ${task.hit_at_10_calls == null ? "—" : `${task.hit_at_10_calls}/5`} | ${number(task.file?.hit_at_1)} | ${number(task.file?.hit_at_5)} | ${number(task.file?.hit_at_10)} | ${number(task.file?.rr_at_10)} | ${number(task.ndcg_at_10)} | ${number(task.measurements?.output_bytes_mean == null ? null : task.measurements.output_bytes_mean / 1024, 2)} | ${number(task.measurements?.latency_ms_mean, 2)} | ${number(task.measurements?.latency_ms_p50, 2)} |`,
      );
    }
    lines.push("", "</details>");
  }
  if (result.failed_tasks.length)
    lines.push(
      "",
      "## Failed tasks",
      "",
      "| Question | Repository | Arm(s) | Failure | Reason |",
      "| --- | --- | --- | --- | --- |",
      ...result.failed_tasks.map(
        (task) =>
          `| ${cell(task.task_id)} | ${cell(task.repository)} | ${task.modes.map((mode) => `zg-${mode}`).join(", ")} | ${cell(task.kind)} | ${cell(task.reason)} |`,
      ),
    );
  if (result.errors.length)
    lines.push(
      "",
      "## Required action",
      "",
      ...result.errors.map((error) => `- ${cell(error)}`),
    );
  if (result.candidate?.commit)
    lines.push(
      "",
      `Rust candidate: \`${cell(result.candidate.ref)}\` at \`${cell(result.candidate.commit)}\`.`,
    );
  if (result.harness_commit)
    lines.push(
      "",
      `Benchmark harness commit: \`${cell(result.harness_commit)}\`.`,
    );
  lines.push(
    "",
    "Details: download the `retrieval-zg-report` artifact for the suite conclusion and the `retrieval-zg-evidence` artifact for per-question public responses. The `retrieval-results` artifact contains the combined page.",
  );
  if (result.run_url)
    lines.push("", `[Open this run and its artifacts](${result.run_url})`);
  const text = `${lines.join("\n")}\n`;
  return result.runtime === "node-0.2.2"
    ? text
        .replaceAll(
          "Rust MCP default presentation",
          "Node.js 0.2.2 MCP default presentation",
        )
        .replaceAll(
          "Rust public MCP default presentation",
          "Node.js 0.2.2 public MCP default presentation",
        )
        .replace("Rust candidate:", "Node.js 0.2.2 candidate:")
    : text;
}

export async function main() {
  const args = {};
  for (let i = 2; i < process.argv.length; i += 2) {
    const key = process.argv[i];
    assert.ok(
      ["--zg", "--output"].includes(key) &&
        process.argv[i + 1] &&
        !Object.hasOwn(args, key.slice(2)),
    );
    args[key.slice(2)] = process.argv[i + 1];
  }
  assert.ok(args.output && args.zg, "requires --zg and --output");
  const maybeRead = async (path) => {
    try {
      return await readJson(path);
    } catch {
      return null;
    }
  };
  const runUrl = process.env.GITHUB_RUN_ID
    ? `${process.env.GITHUB_SERVER_URL}/${process.env.GITHUB_REPOSITORY}/actions/runs/${process.env.GITHUB_RUN_ID}`
    : null;
  const result = await buildCiSummary({
    zg: await maybeRead(args.zg),
    jobResults: JSON.parse(process.env.RETRIEVAL_JOB_RESULTS ?? "{}"),
    runUrl,
    harnessCommit: process.env.RETRIEVAL_HARNESS_COMMIT ?? null,
    candidateCommit: process.env.RETRIEVAL_CANDIDATE_COMMIT ?? null,
    candidateRef: process.env.RETRIEVAL_CANDIDATE_REF ?? null,
  });
  const directory = resolve(args.output);
  await mkdir(directory, { recursive: true });
  await writeJson(join(directory, "summary.json"), result);
  await writeFile(join(directory, "summary.md"), markdownCiSummary(result));
  if (result.status !== "success") process.exitCode = 1;
}
