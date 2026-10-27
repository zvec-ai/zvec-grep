import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { scoreNdcg } from "../metrics/ndcg.mjs";
import {
  FILE_RETRIEVAL_CONTRACT,
  fileRetrievalForRow,
  summarizeFileRetrieval,
} from "../metrics/files.mjs";
import { summarizeMeasurements } from "../metrics/measurements.mjs";
import { markdownPerformance, summarizePerformance } from "./performance.mjs";
import { summarizeNdcg } from "../metrics/summary.mjs";
import { validateZgReport, ZG_MODES } from "./validation.mjs";

const MODES = ZG_MODES;
const HITS = ["hit_at_1", "hit_at_5", "hit_at_10"];
const FILE_METRICS = [...HITS, "rr_at_10"];
const hash = (value) => createHash("sha256").update(value).digest("hex");
const difference = (before, after) => (before === null ? null : after - before);
const key = (row) => JSON.stringify([row.task_id, row.mode]);

function scoreView(row) {
  const targets = row.ndcg.targets;
  const file = fileRetrievalForRow(row);
  return {
    ...Object.fromEntries(
      [
        "task_id",
        "status",
        "execution_status",
        "gold_status",
        "repository",
        "language",
        "items",
      ].map((field) => [field, row[field]]),
    ),
    file_retrieval: file,
    ndcg: file === null ? null : { targets, ...scoreNdcg(row.items, targets) },
    quality_mean: row.quality_mean,
  };
}

function filePair(pairs) {
  const baseline = summarizeFileRetrieval(pairs.map((pair) => pair.baseline));
  const candidate = summarizeFileRetrieval(pairs.map((pair) => pair.candidate));
  return {
    baseline,
    candidate,
    delta: Object.fromEntries(
      [...HITS, "mrr_at_10"].map((field) => [
        field,
        difference(baseline[field], candidate[field]),
      ]),
    ),
  };
}

function ndcgPair(pairs) {
  const baseline = summarizeNdcg(pairs.map((pair) => pair.baseline));
  const candidate = summarizeNdcg(pairs.map((pair) => pair.candidate));
  return {
    baseline,
    candidate,
    delta: Object.fromEntries(
      ["query_mean", "repository_macro", "language_macro"].map(
        (aggregation) => [
          aggregation,
          Object.fromEntries(
            ["ndcg_at_10"].map((metric) => [
              metric,
              difference(
                baseline[aggregation][metric],
                candidate[aggregation][metric],
              ),
            ]),
          ),
        ],
      ),
    ),
  };
}

function reportIdentity(report) {
  const unique = (getter) =>
    [
      ...new Set((report.repositories ?? []).map(getter).filter(Boolean)),
    ].sort();
  return {
    generated_at: report.generated_at ?? null,
    runtime: report.runtime ?? "rust-native",
    integrity_passed: report.integrity_passed,
    product_error_calls: report.product_error_calls,
    candidate_commits: unique((repo) => repo.candidate_commit),
    package_sha256: unique((repo) => repo.package?.tarball_sha256),
    model_files_sha256: unique((repo) => repo.model_files_sha256),
  };
}

/** Compare validated quality rows; task ordering and cached aggregate summaries are not score inputs. */
export function compareReports(baseline, candidate) {
  const before = validateZgReport(baseline, "baseline");
  const after = validateZgReport(candidate, "candidate");
  return compareValidatedReports(baseline, candidate, before, after);
}

function compareValidatedReports(baseline, candidate, before, after) {
  assert.equal(
    baseline.preview,
    candidate.preview,
    "incompatible preview arms",
  );
  for (const field of ["source", "gold", "file_gold", "protocol"])
    assert.equal(
      baseline.suite[field],
      candidate.suite[field],
      `incompatible suite ${field}`,
    );
  assert.equal(baseline.scope, candidate.scope, "incompatible report scopes");
  assert.deepEqual(before.ids, after.ids, "incompatible expected task sets");
  assert.deepEqual(before.modes, after.modes, "incompatible mode sets");
  const modes = MODES.filter((mode) => before.modes.includes(mode));
  const pairs = [];
  for (const mode of modes)
    for (const task_id of baseline.expected_task_ids) {
      const oldRow = before.rows.get(key({ task_id, mode }));
      const newRow = after.rows.get(key({ task_id, mode }));
      for (const field of ["repository", "category", "gold_status", "language"])
        assert.equal(
          oldRow[field],
          newRow[field],
          `${task_id}/${mode}: incompatible ${field}`,
        );
      assert.deepEqual(
        oldRow.ndcg.targets,
        newRow.ndcg.targets,
        `${task_id}/${mode}: incompatible file-target projection`,
      );
      const oldFile =
        fileRetrievalForRow(oldRow) === null
          ? null
          : (oldRow.quality_mean?.file ?? null);
      const newFile =
        fileRetrievalForRow(newRow) === null
          ? null
          : (newRow.quality_mean?.file ?? null);
      assert.equal(
        oldFile === null,
        newFile === null,
        `${task_id}/${mode}: changed scoring eligibility`,
      );
      const fileDelta = Object.fromEntries(
        FILE_METRICS.map((metric) => [
          metric,
          difference(oldFile?.[metric] ?? null, newFile?.[metric] ?? null),
        ]),
      );
      pairs.push({
        task_id,
        mode,
        repository: oldRow.repository,
        category: oldRow.category,
        baseline: scoreView(oldRow),
        candidate: scoreView(newRow),
        file_retrieval_delta: fileDelta,
        ndcg_delta: Object.fromEntries(
          ["ndcg_at_10"].map((metric) => [
            metric,
            oldFile === null
              ? null
              : newRow.quality_mean[metric] - oldRow.quality_mean[metric],
          ]),
        ),
        rank_change:
          fileDelta.rr_at_10 === null
            ? "unscored"
            : fileDelta.rr_at_10 > 0
              ? "improved"
              : fileDelta.rr_at_10 < 0
                ? "regressed"
                : "unchanged",
        status_transition: `${oldRow.status} -> ${newRow.status}`,
        execution_transition: `${oldRow.execution_status} -> ${newRow.execution_status}`,
      });
    }
  return {
    schema_version: 5,
    file_retrieval_contract: FILE_RETRIEVAL_CONTRACT,
    preview: "mcp-default",
    quality_repetition: 5,
    quality_aggregation: "mean_of_five",
    suite: structuredClone(baseline.suite),
    scope: baseline.scope,
    expected_task_ids: [...baseline.expected_task_ids],
    task_order_policy:
      "Task IDs and modes match as sets; tables follow baseline task order and hybrid/fts/vector mode order.",
    quality_gate: "report-only; no quality threshold or causal attribution",
    aggregation:
      "Five metrics recomputed from every repetition's public items on the SWE-QA accepted-file projection: first average five calls per query/mode; file Hit@1/5/10 and MRR@10 then use query means; nDCG@10 uses the repository macro.",
    baseline: reportIdentity(baseline),
    candidate: reportIdentity(candidate),
    warnings: [baseline, candidate].flatMap((report, i) =>
      report.integrity_passed
        ? []
        : [
            `${i === 0 ? "baseline" : "candidate"}: operational integrity failed with ${report.product_error_calls} product-error calls; valid reviewed quality zeros are retained. Differences can include delivery failures and are not pure ranking effects.`,
          ],
    ),
    modes: Object.fromEntries(
      modes.map((mode) => {
        const rows = pairs.filter((pair) => pair.mode === mode);
        return [
          mode,
          {
            measurements: {
              baseline: summarizeMeasurements(
                rows.flatMap(
                  (pair) =>
                    before.rows.get(key(pair)).measurement_observations ?? [],
                ),
              ),
              candidate: summarizeMeasurements(
                rows.flatMap(
                  (pair) =>
                    after.rows.get(key(pair)).measurement_observations ?? [],
                ),
              ),
            },
            file_retrieval: filePair(rows),
            ndcg: ndcgPair(rows),
          },
        ];
      }),
    ),
    tasks: pairs,
    performance: Object.fromEntries(
      [
        ["baseline", baseline],
        ["candidate", candidate],
      ].map(([side, report]) => [
        side,
        summarizePerformance(
          report.repositories.map((repo) => repo.performance ?? {}),
        ),
      ]),
    ),
  };
}

const cell = (value) =>
  String(value ?? "N/A")
    .replaceAll("|", "\\|")
    .replace(/[\r\n]+/g, " ");
const number = (value) =>
  typeof value === "number" ? value.toFixed(4) : "N/A";
const signed = (value) =>
  typeof value === "number"
    ? `${value > 0 ? "+" : ""}${value.toFixed(4)}`
    : "N/A";
const hitCalls = (view) =>
  view.quality_mean ? `${view.quality_mean.hit_at_10_calls}/5` : "N/A";
const stable = (view) =>
  view.quality_mean
    ? view.quality_mean.ranking_repeatable
      ? "Yes"
      : "No"
    : "N/A";

export function markdownComparison(result) {
  const lines = [
    "# zg Retrieval-only version comparison",
    "",
    `Baseline: **${cell(result.baseline.runtime)}** (\`${cell(result.baseline.candidate_commits.join(", "))}\`). Candidate: **${cell(result.candidate.runtime)}** (\`${cell(result.candidate.candidate_commits.join(", "))}\`).`,
    "",
    `Scope: **${result.scope} / ${result.expected_task_ids.length} original questions**. All modes use the Rust public MCP default presentation. Quality averages five calls per query/mode. Delta is candidate minus baseline.`,
    "",
    "Source, Gold, frozen accepted-file targets and protocol identities match; task/mode coverage and scoring eligibility are validated. All five metrics are recomputed from saved public result items. This is a report-only comparison with no quality threshold or causal attribution.",
    "",
    "| Arm / version | File Hit@1 | File Hit@5 | File Hit@10 | File MRR@10 | nDCG@10 | Output mean (KiB) | Avg RT (ms) | P50 RT (ms) |",
    "| --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ...ZG_MODES.flatMap((mode) =>
      ["baseline", "candidate"].map((side) => {
        const entry = result.modes[mode],
          file = entry.file_retrieval[side];
        return `| zg-${mode} / ${side} | ${number(file.hit_at_1)} | ${number(file.hit_at_5)} | ${number(file.hit_at_10)} | ${number(file.mrr_at_10)} | ${number(entry.ndcg[side].repository_macro.ndcg_at_10)} | ${number(entry.measurements[side].output_bytes_mean == null ? null : entry.measurements[side].output_bytes_mean / 1024)} | ${number(entry.measurements[side].latency_ms_mean)} | ${number(entry.measurements[side].latency_ms_p50)} |`;
      }),
    ),
    "",
    "File Hit@1/5/10 and MRR@10 weight original questions equally. nDCG@10 first averages questions within each repository, then weights repositories equally. Native ranks are preserved without deduplication. These SWE-QA accepted-file projection scores measure file localization, not sufficient answer evidence; five repetitions are not independent questions.",
  ];
  const warnings = [...new Set(result.warnings)];
  for (const side of ["baseline", "candidate"])
    if (result.performance?.[side])
      lines.push(
        "",
        `## ${side} performance`,
        "",
        markdownPerformance(result.performance[side]),
      );
  if (warnings.length)
    lines.push(
      "",
      "Operational integrity warnings:",
      "",
      ...warnings.map((warning) => `- ${cell(warning)}`),
    );
  lines.push(
    "",
    "<details>",
    "<summary>Per-question changes</summary>",
    "",
    "| Question / arm | Hit@10 calls (baseline → candidate) | Stable Top 10 (baseline → candidate) | ΔFile Hit@1 | ΔFile Hit@5 | ΔFile Hit@10 | ΔFile RR@10 | ΔnDCG@10 | Execution |",
    "| --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ...result.tasks.map(
      (row) =>
        `| ${cell(row.task_id)} / zg-${cell(row.mode)} | ${hitCalls(row.baseline)} → ${hitCalls(row.candidate)} | ${stable(row.baseline)} → ${stable(row.candidate)} | ${signed(row.file_retrieval_delta.hit_at_1)} | ${signed(row.file_retrieval_delta.hit_at_5)} | ${signed(row.file_retrieval_delta.hit_at_10)} | ${signed(row.file_retrieval_delta.rr_at_10)} | ${signed(row.ndcg_delta.ndcg_at_10)} | ${cell(row.execution_transition)} |`,
    ),
    "",
    "</details>",
    "",
    ...ZG_MODES.flatMap((mode) =>
      ["baseline", "candidate"].map((side) => {
        const entry = result.modes[mode];
        return `zg-${mode} / ${side}: output samples **${entry.measurements[side].output_sample_count}** (successful fifth calls); latency samples **${entry.measurements[side].latency_sample_count}** (all successful valid calls).`;
      }),
    ),
    "",
    "Output means use public UTF-8 bytes / 1024; Avg RT and P50 RT include session load and fixed-order cache effects. Measurements are recomputed from saved per-call metadata, whose raw-response identities are audited by aggregation; this comparison does not reread raw captures. Cross-environment timings do not establish a speed winner.",
    "",
    "Product-error zeros remain in the denominator. Invalid experiments are rejected; unreviewed Gold is N/A. JSON retains input identities, target ranks and per-question status transitions.",
  );
  return (lines.join("\n") + "\n").replaceAll(
    "the Rust public MCP default presentation",
    result.baseline.runtime === result.candidate.runtime
      ? result.baseline.runtime === "node-0.2.2"
        ? "the Node.js 0.2.2 public MCP default presentation"
        : "the Rust public MCP default presentation"
      : "each runtime’s public MCP default presentation",
  );
}

export async function writeComparison(baselinePath, candidatePath, outputPath) {
  const paths = [resolve(baselinePath), resolve(candidatePath)];
  const bytes = await Promise.all(paths.map((path) => readFile(path)));
  const result = compareReports(...bytes.map((raw) => JSON.parse(raw)));
  result.inputs = Object.fromEntries(
    paths.map((path, i) => [
      i === 0 ? "baseline" : "candidate",
      { path, sha256: hash(bytes[i]) },
    ]),
  );
  const output = resolve(outputPath);
  // Validate before creating artifacts; never overwrite a previous comparison.
  await mkdir(output, { recursive: false });
  await writeFile(
    join(output, "comparison.json"),
    `${JSON.stringify(result, null, 2)}\n`,
  );
  await writeFile(join(output, "comparison.md"), markdownComparison(result));
  return result;
}

export async function main() {
  const args = process.argv.slice(2);
  if (args.length !== 3) {
    console.error(
      "Usage: node compare.mjs BASELINE_REPORT.json CANDIDATE_REPORT.json NEW_OUTPUT_DIR",
    );
    process.exitCode = 1;
    return;
  }
  await writeComparison(...args);
  console.log(`Comparison: ${join(resolve(args[2]), "comparison.md")}`);
}
