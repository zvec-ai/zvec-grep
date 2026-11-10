import assert from "node:assert/strict";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { loadSuite } from "../core/lib.mjs";
import { buildCombined } from "./combined.mjs";
import { PILOT_NAMES, PILOT_SUITES } from "./config.mjs";
import { compareReports } from "../reports/compare.mjs";
import { QUALITY_METRICS } from "../reports/ci.mjs";
import {
  QA_ENTITY_METRICS,
  QA_RECALL_METRICS,
  qaMetricCell,
} from "../reports/zg.mjs";

const MODES = ["hybrid", "fts", "vector"];
const identity = (repository, commit) => ({ repository, commit });

export const QA_DIAGNOSTICS = {
  file: { title: "File Recall", metrics: QA_RECALL_METRICS },
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

export function markdownVersionComparison(result) {
  const server = result.server_url ?? "https://github.com";
  const version = (side) => {
    const value = result[side];
    return `[${value.repository}@${value.commit}](${server}/${value.repository}/commit/${value.commit})`;
  };
  const lines = [
    "## Retrieval-only · PR base vs head",
    "",
    `**${result.status === "success" ? "✅ Complete" : "❌ Failed / incomplete"}**`,
    "",
    `Baseline: ${version("baseline")}`,
    "",
    `Candidate: ${version("candidate")}`,
    "",
    "For a PR, the baseline is the pinned target-branch commit and the candidate is the pinned PR commit. In the File ranking table, each cell shows baseline / candidate / Δ (candidate minus baseline) on a 0–1 scale. Both versions use the same frozen harness, queries, Gold and local embedding models, with fresh indexes and five calls per question and mode. Quality scores are report-only.",
    "",
    "| Suite | Mode | File Hit@1 | File Hit@5 | File Hit@10 | MRR@10 | nDCG@10 |",
    "| --- | --- | ---: | ---: | ---: | ---: | ---: |",
  ];
  const cell = ({ baseline, candidate, delta }) =>
    `${baseline.toFixed(4)} / ${candidate.toFixed(4)} / ${delta > 0 ? "+" : ""}${delta.toFixed(4)}`;
  for (const row of result.rows) {
    const label =
      row.suite === "sweqa"
        ? "SWE-QA20"
        : PILOT_SUITES[row.suite].report.overview;
    lines.push(
      `| ${label} | ${row.mode} | ${QUALITY_METRICS.map((metric) => cell(row.metrics[metric])).join(" | ")} |`,
    );
  }
  if (result.rows.some((row) => row.file_diagnostic || row.entity_diagnostic))
    lines.push(
      "",
      "The SWE-QA20 diagnostic tables score File paths and code entities separately. Each cell shows the score and n=scored/eligible questions for that metric. Scores weight each scored question equally after averaging its five calls. Baseline and candidate can have different scored questions.",
    );
  for (const [level, { title, metrics }] of Object.entries(QA_DIAGNOSTICS)) {
    const field = `${level}_diagnostic`;
    const rows = result.rows.filter((row) => row[field]);
    if (!rows.length) continue;
    lines.push(
      "",
      `### SWE-QA20 ${title} · Diagnostic`,
      "",
      `| Mode | Version | ${metrics.map(([label]) => label).join(" | ")} |`,
      `| --- | --- | ${metrics.map(() => "---:").join(" | ")} |`,
    );
    for (const row of rows)
      for (const [side, label] of [
        ["baseline", "Baseline"],
        ["candidate", "Candidate"],
      ]) {
        const group = row[field][side];
        const scores = metrics.map(([, metric]) =>
          qaMetricCell(group?.[level], metric, group?.eligible_tasks),
        );
        lines.push(`| ${row.mode} | ${label} | ${scores.join(" | ")} |`);
      }
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
  lines.push(
    "",
    "SWE-QA20 File nDCG uses repository macro averaging; the other suites use query means. Full base/head reports and per-call evidence are available in the run artifacts.",
  );
  if (result.run_url)
    lines.push("", `[View run and artifacts](${result.run_url})`);
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
