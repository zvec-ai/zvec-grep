import assert from "node:assert/strict";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { loadSuite } from "../core/lib.mjs";
import { validateIndexCounts } from "../core/index-counts.mjs";
import { QA_SCORING } from "../core/suite.mjs";
import { buildCombined } from "./combined.mjs";
import {
  PILOT_FILE_RECALL_METRICS,
  PILOT_NAMES,
  PILOT_QA_SCORING,
} from "./config.mjs";
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
  const hasQa = [baseline.sweqa, candidate.sweqa].some(
    (suite) => suite.qa_scoring || suite.rows?.some((row) => row.qa_retrieval),
  );
  if (hasQa)
    for (const [side, report] of Object.entries({ baseline, candidate }))
      assert.deepEqual(
        report.sweqa.qa_scoring,
        QA_SCORING,
        `${side}: QA scoring rules differ; rescore both reports from saved evidence`,
      );
  const rows = [];
  const pilotQaGold = {};
  const indexCounts = {};
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
    const counts = (suite) => {
      const value = (name === "sweqa" ? suite : suite.report).index_counts;
      return value === undefined ? null : validateIndexCounts(value);
    };
    indexCounts[name] = { baseline: counts(before), candidate: counts(after) };
    const hasPilotQa =
      name !== "sweqa" && (before.report.qa_gold || after.report.qa_gold);
    if (hasPilotQa) {
      assert.deepEqual(
        before.report.qa_gold,
        after.report.qa_gold,
        `${name}: different pilot Gold`,
      );
      for (const pilot of [before, after])
        assert.deepEqual(
          pilot.report.qa_scoring,
          PILOT_QA_SCORING,
          `${name}: different pilot scoring rules`,
        );
      pilotQaGold[name] = before.report.qa_gold;
    }
    for (const mode of MODES) {
      const select = (suite) =>
        name === "sweqa"
          ? suite.rows.find((row) => row.label === `zg-${mode}`)
          : suite.report.summary.find((row) => row.mode === mode);
      const oldRow = select(before),
        newRow = select(after);
      assert.ok(oldRow && newRow, `${name}/${mode}: missing mode`);
      const metrics = {};
      for (const metric of [
        ...QUALITY_METRICS,
        ...(hasPilotQa ? PILOT_FILE_RECALL_METRICS : []),
      ]) {
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
      if (name === "sweqa" || hasPilotQa) {
        for (const level of name === "sweqa"
          ? ["file", "entity"]
          : ["entity"]) {
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
    ...(hasQa ? { qa_scoring: { ...QA_SCORING } } : {}),
    ...(Object.keys(pilotQaGold).length
      ? {
          pilot_qa_gold: pilotQaGold,
          pilot_qa_scoring: { ...PILOT_QA_SCORING },
        }
      : {}),
    harness_commit: baseline.harness_commit,
    index_counts: indexCounts,
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
  const beforeCell = cell(before),
    afterCell = cell(after);
  // Preserve score changes hidden by rounding. Counts stay in the JSON report.
  const same =
    beforeCell === afterCell &&
    (beforeCell === "N/A" ||
      before?.[level]?.[metric] === after?.[level]?.[metric]);
  return same ? beforeCell : `${beforeCell} → ${afterCell}`;
}

function indexCountComparison(pair) {
  const count = (value) =>
    Number.isSafeInteger(value) && value >= 0
      ? value.toLocaleString("en-US")
      : "N/A";
  const cell = (key) => {
    const baseline = pair?.baseline?.[key] ?? null;
    const candidate = pair?.candidate?.[key] ?? null;
    return baseline === candidate
      ? count(baseline)
      : `${count(baseline)} → ${count(candidate)}`;
  };
  return `Indexed corpus: **${cell("entities")} Entities · ${cell("fragments")} Fragments**.`;
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
    "Indexed corpus totals count each independently built index once; modes and repetitions do not multiply them. N/A means the count was not recorded.",
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
      const pilotQa = result.rows.some(
        (row) => row.suite === suite && row.entity_diagnostic,
      );
      lines.push(
        "",
        `### ${title}`,
        "",
        indexCountComparison(result.index_counts?.[suite]),
        "",
        qa
          ? "**Diagnostic QA results.** Entity hits require Gold containment in the returned Entity, independently of preview. Cells show percentages. N/A means unscored or missing QA data."
          : pilotQa
            ? "**Diagnostic Entity results.** File metrics use the original positive paths. Entity metrics use verified evidence spans or preimage functions and may cover fewer questions. N/A means unscored."
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
            qa || (pilotQa && level === "entity")
              ? qaComparisonCell(row?.[`${level}_diagnostic`], level, metric)
              : level === "file"
                ? comparisonCell(
                    row?.metrics?.[fileMetrics[metric] ?? `file_${metric}`],
                  )
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
