import assert from "node:assert/strict";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { loadSuite } from "../core/lib.mjs";
import { buildCombined } from "./combined.mjs";
import { PILOT_NAMES } from "./config.mjs";
import { compareReports } from "../reports/compare.mjs";
import { QUALITY_METRICS } from "../reports/ci.mjs";
import {
  QA_ENTITY_METRICS,
  QA_RECALL_METRICS,
  qaPercentCell,
} from "../reports/zg.mjs";

const MODES = ["hybrid", "fts", "vector"];
const identity = (repository, commit) => ({ repository, commit });

export const QA_DIAGNOSTICS = {
  file: {
    title: "File retrieval",
    metrics: QA_RECALL_METRICS,
    optionalMetrics: QA_ENTITY_METRICS.filter(
      ([, metric]) => !metric.startsWith("recall_"),
    ),
  },
  entity: { title: "Entity retrieval", metrics: QA_ENTITY_METRICS },
};

function diagnosticFor(row, level) {
  const group = row.qa_retrieval?.diagnostic;
  return group?.[level]
    ? { eligible_tasks: group.eligible_tasks, [level]: group[level] }
    : null;
}

function checkIdentity(value) {
  assert.match(value.repository ?? "", /^[\w.-]+\/[\w.-]+$/);
  assert.match(value.commit ?? "", /^[a-f0-9]{40}$/);
}

// Inputs to this function have already been rebuilt from validated suite rows.
export function compareCombinedResults(baseline, candidate, metadata) {
  for (const side of ["baseline", "candidate"]) {
    checkIdentity(metadata[side]);
    const report = side === "baseline" ? baseline : candidate;
    assert.equal(report.status, "success", `${side}: incomplete results`);
    assert.equal(
      report.candidate.commit,
      metadata[side].commit,
      `${side}: wrong commit`,
    );
    assert.match(report.harness_commit ?? "", /^[a-f0-9]{40}$/);
  }
  assert.equal(
    baseline.harness_commit,
    candidate.harness_commit,
    "different benchmark harnesses",
  );
  const rows = [];
  for (const name of ["sweqa", ...PILOT_NAMES]) {
    const before = name === "sweqa" ? baseline.sweqa : baseline.pilots[name];
    const after = name === "sweqa" ? candidate.sweqa : candidate.pilots[name];
    assert.equal(before.status, "success", `${name}: baseline incomplete`);
    assert.equal(after.status, "success", `${name}: candidate incomplete`);
    for (const suite of [before, after])
      assert.equal(
        name === "sweqa" ? suite.preview : suite.report.preview,
        "full",
        `${name}: full preview required for both versions`,
      );
    assert.equal(
      before.model,
      after.model,
      `${name}: different embedding models`,
    );
    for (const mode of MODES) {
      const select = (suite) =>
        name === "sweqa"
          ? suite.rows.find((row) => row.label === `zg-${mode}`)
          : suite.report.summary.find((row) => row.mode === mode);
      const oldRow = select(before),
        newRow = select(after);
      assert.ok(oldRow && newRow, `${name}/${mode}: missing mode`);
      const metrics = {};
      for (const metric of QUALITY_METRICS) {
        const a = oldRow.metrics?.[metric],
          b = newRow.metrics?.[metric];
        assert.ok(
          [a, b].every(
            (value) => Number.isFinite(value) && value >= 0 && value <= 1,
          ),
          `${name}/${mode}: invalid ${metric}`,
        );
        metrics[metric] = { baseline: a, candidate: b, delta: b - a };
      }
      const row = { suite: name, mode, model: before.model, metrics };
      if (name === "sweqa") {
        for (const level of Object.keys(QA_DIAGNOSTICS)) {
          const baseline = diagnosticFor(oldRow, level);
          const candidate = diagnosticFor(newRow, level);
          if (baseline || candidate)
            row[`${level}_diagnostic`] = { baseline, candidate };
        }
      }
      rows.push(row);
    }
  }
  return {
    schema_version: 1,
    kind: "retrieval-version-comparison",
    status: "success",
    preview: "full",
    ...metadata,
    harness_commit: baseline.harness_commit,
    rows,
    errors: [],
  };
}

export async function compareRuns(baseline, candidate, metadata) {
  const rebuild = async (bundle, side) => {
    const expected = metadata[side];
    assert.equal(bundle.summary.status, "success", `${side}: run incomplete`);
    assert.equal(
      bundle.summary.candidate.commit,
      expected.commit,
      `${side}: wrong artifact commit`,
    );
    assert.ok(
      bundle.reports.zg?.repositories?.length,
      `${side}: missing repositories`,
    );
    for (const repository of bundle.reports.zg.repositories)
      assert.equal(
        repository.candidate_commit,
        expected.commit,
        `${side}: wrong SWE-QA candidate`,
      );
    return buildCombined({
      ...bundle.reports,
      candidateCommit: expected.commit,
      candidateRef: expected.commit,
      harnessCommit: bundle.summary.harness_commit,
    });
  };
  const before = await rebuild(baseline, "baseline");
  const after = await rebuild(candidate, "candidate");
  // Validate equal source, protocol, file targets, task sets and scoring eligibility.
  // This also recomputes the SWE-QA scores from every saved repetition.
  compareReports(baseline.reports.zg, candidate.reports.zg, {
    suite: await loadSuite(),
  });
  return compareCombinedResults(before, after, metadata);
}

const percentage = (value) => `${(value * 100).toFixed(2)}%`;

function comparisonCell(values) {
  const baseline = values?.baseline;
  const candidate = values?.candidate;
  const cell = (value) => (Number.isFinite(value) ? percentage(value) : "N/A");
  return baseline === candidate
    ? cell(baseline)
    : `${cell(baseline)} → ${cell(candidate)}`;
}

function qaComparisonCell(pair, level, metric) {
  const before = pair?.baseline;
  const after = pair?.candidate;
  const cell = (group) =>
    qaPercentCell(group?.[level], metric, group?.eligible_tasks);
  // Equal displayed text is insufficient: preserve changes hidden by rounding,
  // and equal scores with different scored or eligible question counts.
  const same =
    before?.eligible_tasks === after?.eligible_tasks &&
    before?.[level]?.[metric] === after?.[level]?.[metric] &&
    before?.[level]?.metric_task_counts?.[metric] ===
      after?.[level]?.metric_task_counts?.[metric];
  return same ? cell(before) : `${cell(before)} → ${cell(after)}`;
}

export function markdownVersionComparison(result) {
  const server = result.server_url ?? "https://github.com";
  const version = (side) => {
    const value = result[side];
    return `[${value.commit.slice(0, 8)}](${server}/${value.repository}/commit/${value.commit})`;
  };
  const hasQa = result.rows.some(
    (row) =>
      row.suite === "sweqa" && (row.file_diagnostic || row.entity_diagnostic),
  );
  const lines = [
    "## Retrieval-only",
    "",
    `**${result.status === "success" ? "✅ Complete" : "❌ Failed / incomplete"}** · Baseline ${version("baseline")} · Candidate ${version("candidate")}`,
    "",
    "Scores are percentages. Different values show **baseline → candidate**; identical values are shown once. Both versions use the same queries, Gold and local embedding models, with five calls per question and mode.",
  ];
  if (result.preview === "full")
    lines.push("", 'Both versions explicitly request `preview: "full"`.');
  if (result.rows.length) {
    const fileMetrics = {
      hit_at_1: "file_hit_at_1",
      hit_at_5: "file_hit_at_5",
      hit_at_10: "file_hit_at_10",
      mrr_at_10: "file_mrr_at_10",
      ndcg_at_10: "ndcg_at_10",
    };
    for (const [suite, title] of [
      ["sweqa", "SWE-QA20"],
      ["beir", "BEIR"],
      ["duretrieval", "DuRetrieval"],
      ["quarry", "Quarry"],
    ]) {
      const qa = suite === "sweqa" && hasQa;
      lines.push(
        "",
        `### ${title}`,
        "",
        qa
          ? "**Diagnostic QA results.** Cells show score (scored/eligible questions) for each metric. A shared cell means equal scores and counts, not necessarily the same scored questions. N/A means unscored or missing QA data; original File scores are not substituted."
          : "Original File metrics. File Recall and Entity evaluation data are not provided for this run and are shown as N/A.",
        "",
        `| Mode | Level | ${QA_ENTITY_METRICS.map(([label]) => label).join(" | ")} |`,
        `| --- | --- | ${QA_ENTITY_METRICS.map(() => "---:").join(" | ")} |`,
      );
      for (const mode of ["fts", "vector", "hybrid"]) {
        const row = result.rows.find(
          (item) => item.suite === suite && item.mode === mode,
        );
        for (const [level, label] of [
          ["file", "File"],
          ["entity", "Entity"],
        ]) {
          const scores = QA_ENTITY_METRICS.map(([, metric]) =>
            qa
              ? qaComparisonCell(row?.[`${level}_diagnostic`], level, metric)
              : level === "file" && fileMetrics[metric]
                ? comparisonCell(row?.metrics?.[fileMetrics[metric]])
                : "N/A",
          );
          lines.push(`| ${mode} | ${label} | ${scores.join(" | ")} |`);
        }
      }
    }
    lines.push(
      "",
      "Original File metrics remain unchanged in the JSON and full reports. QA metrics average questions equally; original SWE-QA20 File nDCG averages repositories, while the other suites average queries. Quality scores are report-only.",
    );
  }
  if (result.errors.length)
    lines.push(
      "",
      "No complete comparison is available. Missing or invalid results are not zero scores.",
      "",
      ...result.errors.map(
        (error) =>
          `- ${String(error)
            .replaceAll("|", "\\|")
            .replace(/[\r\n]+/g, " ")}`,
      ),
    );
  if (result.run_url)
    lines.push("", `[Full reports and per-call evidence](${result.run_url})`);
  return `${lines.join("\n")}\n`;
}

export async function main(args = process.argv.slice(2)) {
  const { values } = parseArgs({
    args,
    options: Object.fromEntries(
      ["baseline", "candidate", "output"].map((key) => [
        key,
        { type: "string" },
      ]),
    ),
  });
  assert.ok(values.baseline && values.candidate && values.output);
  const env = process.env;
  const metadata = {
    baseline: identity(
      env.RETRIEVAL_BASELINE_REPOSITORY,
      env.RETRIEVAL_BASELINE_COMMIT,
    ),
    candidate: identity(
      env.RETRIEVAL_CANDIDATE_REPOSITORY,
      env.RETRIEVAL_CANDIDATE_COMMIT,
    ),
    server_url: env.GITHUB_SERVER_URL ?? "https://github.com",
    run_url: env.GITHUB_RUN_ID
      ? `${env.GITHUB_SERVER_URL}/${env.GITHUB_REPOSITORY}/actions/runs/${env.GITHUB_RUN_ID}`
      : null,
  };
  let result;
  try {
    for (const side of ["baseline", "candidate"]) checkIdentity(metadata[side]);
    const jobs = JSON.parse(env.RETRIEVAL_COMPARISON_JOBS ?? "{}");
    if (Object.keys(jobs).length)
      for (const side of ["baseline", "candidate"])
        assert.equal(
          jobs[side]?.result,
          "success",
          `${side}: benchmark job failed or was cancelled`,
        );
    const read = async (directory) => ({
      summary: JSON.parse(
        await readFile(join(directory, "summary.json"), "utf8"),
      ),
      reports: Object.fromEntries(
        await Promise.all(
          ["zg", ...PILOT_NAMES].map(async (name) => [
            name,
            JSON.parse(
              await readFile(
                join(directory, "reports", `${name}.json`),
                "utf8",
              ),
            ),
          ]),
        ),
      ),
    });
    result = await compareRuns(
      await read(values.baseline),
      await read(values.candidate),
      metadata,
    );
  } catch (error) {
    result = {
      schema_version: 1,
      kind: "retrieval-version-comparison",
      status: "failed",
      ...metadata,
      rows: [],
      errors: [error.message],
    };
  }
  await mkdir(resolve(values.output), { recursive: true });
  await writeFile(
    join(values.output, "summary.json"),
    `${JSON.stringify(result, null, 2)}\n`,
  );
  await writeFile(
    join(values.output, "summary.md"),
    markdownVersionComparison(result),
  );
  if (result.status !== "success") process.exitCode = 1;
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
)
  await main();
