import assert from "node:assert/strict";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";
import {
  buildCiSummary,
  markdownCiSummary,
  QUALITY_METRICS,
} from "../reports/ci.mjs";
import {
  FILE_RETRIEVAL_CONTRACT,
  fileRetrievalForRow,
  summarizeFileRetrieval,
} from "../metrics/files.mjs";
import { loadSuite } from "../core/lib.mjs";
import { compareReports } from "../reports/compare.mjs";
import { summarizeMeasurements } from "../metrics/measurements.mjs";
import { summarizeNdcg } from "../metrics/summary.mjs";
import { scoreNdcg } from "../metrics/ndcg.mjs";
import { summarizeRepeatedQuality } from "../metrics/repetitions.mjs";
import {
  publicQaItems,
  scoreQaRetrieval,
  summarizeRepeatedQaRetrieval,
  summarizeQaRetrieval,
} from "../metrics/entities.mjs";
import { validateZgReport, ZG_MODES } from "../reports/validation.mjs";
import { summarizeIndexCounts } from "../core/index-counts.mjs";

const suite = await loadSuite();
const clone = (value) => structuredClone(value);
function qualityRow(task, mode, { failed = false, firstRank = 3 } = {}) {
  const targets = suite.file_gold[task.task_id].targets;
  const items = failed
    ? []
    : Array.from({ length: 10 }, (_, index) => ({
        rank: index + 1,
        path:
          index + 1 === firstRank
            ? targets[0].path
            : `__unrelated__/entry_${index + 1}.none`,
        range: { kind: "text", start_line: 100, end_line: 120 },
        matched_by: mode === "hybrid" ? "fts+vector" : mode,
      }));
  const row = {
    task_id: task.task_id,
    mode,
    preview: "full",
    repetition: 5,
    quality_observation: true,
    repository: task.repository,
    category: task.category,
    language: suite.file_gold[task.task_id].language,
    gold_status: suite.gold[task.task_id].status,
    status: failed ? "product_error" : "scored",
    execution_status: failed ? "product_error" : "success",
    latency_ms: failed ? null : 12.34567,
    visible_output_bytes: failed ? null : 3584,
    items,
    ndcg: { targets, ...scoreNdcg(items, targets) },
  };
  row.file_retrieval = fileRetrievalForRow(row);
  row.measurement_observations = Array.from({ length: 5 }, (_, index) => ({
    repetition: index + 1,
    status: row.status,
    execution_status: row.execution_status,
    latency_ms: row.latency_ms,
    visible_output_bytes: row.visible_output_bytes,
  }));
  row.quality_observations = Array.from({ length: 5 }, (_, index) => ({
    repetition: index + 1,
    items: clone(items),
  }));
  row.quality_mean = summarizeRepeatedQuality(
    row.quality_observations,
    targets,
  );
  row.ranking_repeatable = failed ? null : true;
  return row;
}
function modeReport(rows) {
  return {
    file_retrieval: summarizeFileRetrieval(rows),
    ndcg: summarizeNdcg(rows),
    measurements: summarizeMeasurements(
      rows.flatMap((row) => row.measurement_observations),
    ),
  };
}
function zgReport({ failedModes = [], rankForTask = () => 3 } = {}) {
  const tasks = ZG_MODES.flatMap((mode) =>
    suite.lock.tasks.map((task) =>
      qualityRow(task, mode, {
        failed: failedModes.includes(mode),
        firstRank: rankForTask(task, mode),
      }),
    ),
  );
  return {
    schema_version: 7,
    file_retrieval_contract: FILE_RETRIEVAL_CONTRACT,
    preview: "full",
    quality_repetition: 5,
    quality_aggregation: "mean_of_five",
    quality_score_valid: true,
    integrity_passed: failedModes.length === 0,
    integrity_errors: [],
    product_error_calls: failedModes.length * 100,
    suite: clone(suite.identity),
    scope: "full-20-original-queries",
    expected_task_ids: suite.lock.tasks.map((task) => task.task_id),
    observed_calls: 300,
    modes: Object.fromEntries(
      ZG_MODES.map((mode) => [
        mode,
        modeReport(tasks.filter((row) => row.mode === mode)),
      ]),
    ),
    tasks,
    repositories: suite.lock.repositories.map((repository) => ({
      repository: repository.repository,
    })),
  };
}

function withQa(report = zgReport()) {
  report.qa_gold = clone(suite.qa_gold);
  report.qa_scoring = clone(suite.qa_scoring);
  for (const row of report.tasks) {
    const pair = suite.qa_pairs[row.task_id];
    row.qa_observations = row.quality_observations.map(
      (observation, index) => ({
        repetition: observation.repetition,
        items: publicQaItems(observation.items),
        status: row.measurement_observations[index].status,
        execution_status: row.measurement_observations[index].execution_status,
      }),
    );
    row.qa_retrieval = scoreQaRetrieval(row.items, pair, {
      status: row.status,
      executionStatus: row.execution_status,
    });
    row.qa_quality_mean = summarizeRepeatedQaRetrieval(
      row.qa_observations,
      pair,
    );
  }
  for (const mode of ZG_MODES)
    report.modes[mode].qa_retrieval = summarizeQaRetrieval(
      report.tasks.filter((row) => row.mode === mode),
    );
  return report;
}

test("SWE-QA inventory sums each pinned repository once across all calls", async () => {
  const report = zgReport();
  for (const [index, manifest] of report.repositories.entries()) {
    const repo = suite.lock.repositories[index];
    manifest.index_counts = summarizeIndexCounts([
      {
        id: `${repo.repository}@${repo.commit}`,
        entities: 2,
        fragments: 5,
      },
    ]);
  }
  const summary = await buildCiSummary({ zg: report });
  assert.equal(summary.status, "success");
  assert.equal(summary.index_counts.entities, 22);
  assert.equal(summary.index_counts.fragments, 55);
  assert.match(markdownCiSummary(summary), /22 Entities · 55 Fragments/);
  delete report.repositories[0].index_counts;
  const missing = await buildCiSummary({ zg: report });
  assert.equal(missing.status, "success");
  assert.equal(missing.index_counts.entities, null);
  assert.equal(missing.index_counts.fragments, null);
});
function assertUnavailable(result) {
  assert.equal(result.status, "failed");
  assert.deepEqual(
    result.rows.map((row) => row.label),
    ["zg-hybrid", "zg-fts", "zg-vector"],
  );
  for (const row of result.rows) {
    assert.equal(row.status, "invalid");
    assert.equal(row.questions, 0);
    assert.equal(row.expected_questions, 20);
    assert.equal(row.metrics, null);
    assert.equal(row.measurements, null);
  }
}

test("summary schema 5 has exactly the fixed three Rust MCP ZG arms", async () => {
  const result = await buildCiSummary({ zg: zgReport() });
  assert.equal(result.schema_version, 5);
  assert.equal(result.status, "success");
  assert.equal(result.preview, "full");
  assert.deepEqual(result.quality_metrics, [
    "file_hit_at_1",
    "file_hit_at_5",
    "file_hit_at_10",
    "file_mrr_at_10",
    "ndcg_at_10",
  ]);
  assert.deepEqual(
    result.rows.map((row) => row.label),
    ["zg-hybrid", "zg-fts", "zg-vector"],
  );
  for (const row of result.rows) {
    assert.equal(row.questions, 20);
    assert.equal(row.repositories, 11);
    assert.ok(Math.abs(row.metrics.file_mrr_at_10 - 1 / 3) < 1e-14);
    assert.equal(row.measurements.output_sample_count, 20);
    assert.equal(row.measurements.latency_sample_count, 100);
  }
  assert.equal(result.tasks.length, 60);
  assert.ok(result.tasks.every((task) => task.status === "scored"));
  assert.match(
    markdownCiSummary(result),
    /Per-question results \(60 question\/mode rows\)/,
  );
  assert.deepEqual(result.quality_metrics, QUALITY_METRICS);
  assert.doesNotMatch(
    JSON.stringify(result),
    /comparison|previews|primary_preview/,
  );
});

test("CI exposes diagnostic QA scores and denominators while formal scores remain N/A", async () => {
  const report = withQa();
  const result = await buildCiSummary({ zg: report });
  assert.equal(result.status, "success", result.errors.join("\n"));
  assert.deepEqual(result.qa_gold, suite.qa_gold);
  assert.deepEqual(result.qa_scoring, { version: 2, entity_match: "contains" });
  for (const [index, mode] of ZG_MODES.entries()) {
    const qa = result.rows[index].qa_retrieval;
    assert.deepEqual(qa, report.modes[mode].qa_retrieval);
    assert.equal(qa.planned_tasks, 20);
    assert.equal(qa.formal.eligible_tasks, 0);
    assert.equal(qa.formal.entity.recall_at_10, null);
    assert.equal(qa.formal.entity.metric_task_counts.recall_at_10, 0);
    assert.ok(qa.diagnostic.eligible_tasks > 0);
    assert.ok(qa.diagnostic.file.metric_task_counts.recall_at_10 > 0);
    assert.ok(result.rows[index].metrics.file_mrr_at_10 > 0);
  }
  assert.ok(
    result.tasks.some((task) => task.qa_quality_mean?.status === "diagnostic"),
  );
  const text = markdownCiSummary(result);
  assert.match(text, /Scope \| Arm \| Level \| Recall@1/);
  assert.match(text, /All File and Entity metrics/);
  assert.match(text, /Formal QA scores are unavailable/);
  assert.match(text, /\| Diagnostic \| zg-hybrid \|/);
  assert.doesNotMatch(text, /scored\/eligible questions/);
  assert.doesNotMatch(text, /(?:%|N\/A) \([^)]*\/[^)]*\)/);
  assert.match(text, /do not establish formal Entity performance/);
  assert.deepEqual(result.quality_metrics, QUALITY_METRICS);
});

test("CI rejects altered QA provenance, scores, eligibility, observations and aggregates", async () => {
  const mutations = [
    (report) => {
      delete report.qa_scoring;
    },
    (report) => {
      report.qa_scoring = { entity_match: "identity" };
    },
    (report) => {
      report.qa_gold = { ...report.qa_gold, forged: true };
    },
    (report, row) => {
      row.qa_retrieval.entity.recall_at_10 = 999;
    },
    (report, row) => {
      row.qa_quality_mean.entity.recall_at_10 = 999;
    },
    (report, row) => {
      row.qa_retrieval.formal_eligible = true;
    },
    (report, row) => {
      row.qa_quality_mean.formal_eligible = true;
    },
    (report, row) => {
      row.qa_observations.pop();
    },
    (report, row) => {
      row.qa_observations[0].items[0].rank = 2;
    },
    (report, row) => {
      row.qa_observations[0].execution_status = "product_error";
    },
    (report, row) => {
      row.qa_observations[4].items[0].metadata = ["forged evidence"];
    },
    (report) => {
      report.modes.hybrid.qa_retrieval.formal.eligible_tasks = 1;
    },
    (report) => {
      report.modes.hybrid.qa_retrieval.diagnostic.entity.recall_at_10 = 999;
    },
  ];
  for (const mutate of mutations) {
    const report = withQa();
    const row = report.tasks.find((task) => task.qa_retrieval?.entity);
    assert.ok(row, "fixture needs a diagnostic QA question");
    mutate(report, row);
    assertUnavailable(await buildCiSummary({ zg: report }));
  }
});

test("QA comparisons bind matching rules independently of unchanged Gold", () => {
  const baseline = withQa();
  const candidate = clone(baseline);
  const result = compareReports(baseline, candidate, { suite });
  assert.deepEqual(result.qa_gold, suite.qa_gold);
  assert.deepEqual(result.qa_scoring, { version: 2, entity_match: "contains" });
  for (const scoring of [
    undefined,
    { version: 1, entity_match: "identity" },
    { version: 1, entity_match: "contains" },
  ]) {
    const legacy = clone(candidate);
    if (scoring) legacy.qa_scoring = scoring;
    else delete legacy.qa_scoring;
    // Unchanged numeric scores cannot make older matching rules compatible.
    assert.throws(
      () => compareReports(baseline, legacy, { suite }),
      /QA scoring rules differ/,
    );
  }
  assert.throws(
    () => compareReports(zgReport(), candidate, { suite }),
    /incompatible QA Gold/,
  );
});

test("QA failed calls remain unscored without changing historical File error zeros", async () => {
  const result = await buildCiSummary({
    zg: withQa(zgReport({ failedModes: ["vector"] })),
  });
  assert.equal(result.status, "failed");
  const vector = result.rows.find((row) => row.label === "zg-vector");
  assert.equal(vector.metrics.file_hit_at_10, 0);
  assert.equal(vector.qa_retrieval.formal.eligible_tasks, 0);
  assert.equal(vector.qa_retrieval.diagnostic.entity.recall_at_10, null);
  assert.equal(
    vector.qa_retrieval.diagnostic.entity.metric_task_counts.recall_at_10,
    0,
  );
  assert.ok(vector.qa_retrieval.diagnostic.eligible_tasks > 0);
  assert.match(markdownCiSummary(result), /\| N\/A \|/);
});

test("QA coverage keeps invalid questions in the planned denominator", async () => {
  const report = withQa();
  const row = report.tasks.find(
    (task) =>
      task.mode === "hybrid" && task.qa_quality_mean?.status === "diagnostic",
  );
  const previous = report.modes.hybrid.qa_retrieval;
  row.status = "harness_invalid";
  row.execution_status = "harness_invalid";
  row.invalid_reason = "format_unknown: public source range mismatch";
  row.ndcg = null;
  row.file_retrieval = null;
  row.qa_retrieval = null;
  row.qa_quality_mean = null;
  row.items = [];
  row.measurement_observations = row.measurement_observations.map((sample) => ({
    ...sample,
    status: "harness_invalid",
    execution_status: "harness_invalid",
  }));
  report.quality_score_valid = false;
  report.integrity_passed = false;
  report.integrity_errors = [
    "one or more observations are experimentally invalid",
  ];
  report.modes.hybrid.qa_retrieval = summarizeQaRetrieval(
    report.tasks.filter((task) => task.mode === "hybrid"),
  );
  const result = await buildCiSummary({ zg: report });
  const hybrid = result.rows[0];
  assert.equal(hybrid.status, "partial", result.errors.join("\n"));
  assert.equal(hybrid.questions, 19);
  assert.equal(hybrid.qa_retrieval.planned_tasks, 20);
  assert.equal(
    hybrid.qa_retrieval.diagnostic.eligible_tasks,
    previous.diagnostic.eligible_tasks - 1,
  );
  assert.equal(
    hybrid.qa_retrieval.unavailable_tasks.length,
    previous.unavailable_tasks.length + 1,
  );
  assert.ok(
    hybrid.qa_retrieval.unavailable_tasks.some(
      (task) => task.task_id === row.task_id,
    ),
  );
});

test("isolated invalid question is excluded from partial scores and named in the failed-task table", async () => {
  const report = zgReport();
  const taskId = suite.lock.tasks.find(
    (task) => task.repository === "pylint-dev/pylint",
  ).task_id;
  for (const row of report.tasks.filter((row) => row.task_id === taskId)) {
    row.status = "harness_invalid";
    row.execution_status = "harness_invalid";
    row.invalid_reason = "format_unknown: public source range mismatch";
    row.ndcg = null;
    row.file_retrieval = null;
    row.items = [];
    row.measurement_observations = row.measurement_observations.map(
      (sample) => ({
        ...sample,
        status: "harness_invalid",
        execution_status: "harness_invalid",
      }),
    );
  }
  report.quality_score_valid = false;
  report.integrity_passed = false;
  report.integrity_errors = [
    "one or more observations are experimentally invalid",
  ];
  const result = await buildCiSummary({ zg: report });
  assert.equal(result.status, "failed");
  assert.deepEqual(
    result.rows.map((row) => row.status),
    ["partial", "partial", "partial"],
  );
  assert.ok(result.rows.every((row) => row.questions === 19));
  assert.ok(result.rows.every((row) => row.metrics.file_hit_at_5 === 1));
  assert.deepEqual(result.failed_tasks, [
    {
      task_id: taskId,
      repository: "pylint-dev/pylint",
      modes: ["hybrid", "fts", "vector"],
      kind: "invalid_evidence",
      reason: "format_unknown: public source range mismatch",
    },
  ]);
  assert.equal(result.tasks.length, 60);
  assert.equal(
    result.tasks.filter((row) => row.status === "invalid_evidence").length,
    3,
  );
  const markdown = markdownCiSummary(result);
  assert.match(markdown, /19\/20 questions; 11\/11 repositories/);
  assert.match(markdown, /pylint-dev\/pylint/);
  assert.match(markdown, /public source range mismatch/);
  assert.match(markdown, /Partial scores are diagnostic/);
  assert.doesNotMatch(markdown, /0\/20 questions; 0\/11 repositories/);

  report.integrity_errors.unshift("mixed candidate tarball_sha256");
  assertUnavailable(await buildCiSummary({ zg: report }));
  report.integrity_errors.shift();
  report.tasks.find((row) => row.task_id === taskId).file_retrieval = {
    hit_at_1: 1,
  };
  assertUnavailable(await buildCiSummary({ zg: report }));
});

test("missing and invalid reports withhold all three arms instead of fabricating zeros", async () => {
  assertUnavailable(await buildCiSummary());
  const report = zgReport();
  report.quality_score_valid = false;
  assertUnavailable(await buildCiSummary({ zg: report }));
});

test("CI rejects old contracts and incomplete or misrouted three-mode matrices", async () => {
  for (const mutate of [
    (r) => {
      r.schema_version = 5;
    },
    (r) => {
      r.preview = "short";
    },
    (r) => {
      r.previews = {};
    },
    (r) => {
      r.primary_preview = "full";
    },
    (r) => {
      r.tasks[0].preview = "short";
    },
    (r) => {
      r.tasks.pop();
    },
    (r) => {
      r.tasks[1] = clone(r.tasks[0]);
    },
    (r) => {
      delete r.modes.vector;
    },
    (r) => {
      r.modes.extra = {};
    },
    (r) => {
      r.observed_calls = 299;
    },
    (r) => {
      r.tasks.find((row) => row.mode === "vector").items[0].matched_by =
        "fts+vector";
    },
    (r) => {
      r.tasks.find((row) => row.mode === "fts").items[0].matched_by = "vector";
    },
  ]) {
    const report = zgReport();
    mutate(report);
    assertUnavailable(await buildCiSummary({ zg: report }));
  }
});

test("per-question scores and frozen label identities cannot be changed", async () => {
  for (const mutate of [
    (r) => {
      r.tasks[0].file_retrieval.mrr_at_10 = 999;
    },
    (r) => {
      r.tasks[0].ndcg.ndcg_at_10 = 999;
    },
    (r) => {
      r.tasks[0].ndcg.targets = [{ path: "changed.py" }];
    },
    (r) => {
      r.tasks[0].repository = "forged/repository";
    },
    (r) => {
      r.tasks[0].language = "forged-language";
    },
    (r) => {
      r.tasks[0].category = "invalid";
    },
    (r) => {
      r.tasks[0].gold_status = "unknown";
    },
    (r) => {
      r.suite.protocol = "f".repeat(64);
    },
  ]) {
    const report = zgReport();
    mutate(report);
    assertUnavailable(await buildCiSummary({ zg: report }));
  }
});

test("cached aggregates never override public items or per-call measurements", async () => {
  const report = zgReport(),
    snapshot = clone(report);
  for (const mode of ZG_MODES)
    report.modes[mode] = {
      file_retrieval: { mrr_at_10: 999 },
      ndcg: { repository_macro: { ndcg_at_10: 999 } },
      measurements: { latency_ms_p50: 999 },
    };
  const result = await buildCiSummary({ zg: report });
  assert.equal(result.status, "success");
  for (const [index, mode] of ZG_MODES.entries()) {
    assert.equal(
      result.rows[index].metrics.file_mrr_at_10,
      snapshot.modes[mode].file_retrieval.mrr_at_10,
    );
    assert.equal(
      result.rows[index].metrics.ndcg_at_10,
      snapshot.modes[mode].ndcg.repository_macro.ndcg_at_10,
    );
    assert.equal(result.rows[index].measurements.latency_ms_p50, 12.34567);
  }
});

test("measurement evidence must cover all five repetitions with valid successful calls", async () => {
  for (const mutate of [
    (r) => {
      r.tasks[0].measurement_observations.pop();
    },
    (r) => {
      r.tasks[0].measurement_observations[0].repetition = 5;
    },
    ...[null, -1, Number.NaN].map((value) => (r) => {
      r.tasks[0].measurement_observations[0].latency_ms = value;
    }),
    (r) => {
      r.tasks[0].measurement_observations[0].visible_output_bytes = -1;
    },
    (r) => {
      r.tasks[0].measurement_observations[4].latency_ms = 999;
    },
  ]) {
    const report = zgReport();
    mutate(report);
    assert.throws(
      () => validateZgReport(report, "ZG", { suite }),
      /measurement|successful-call/,
    );
    assertUnavailable(await buildCiSummary({ zg: report }));
  }
});

test("product failures retain quality zeros only in the affected arm and exclude error text from measurements", async () => {
  const report = zgReport({ failedModes: ["vector"] });
  for (const row of report.tasks.filter((row) => row.mode === "vector")) {
    row.visible_output_bytes = 37;
    for (const sample of row.measurement_observations)
      sample.visible_output_bytes = 37;
  }
  const result = await buildCiSummary({ zg: report });
  assert.equal(result.status, "failed");
  assert.deepEqual(
    result.rows.map((row) => row.status),
    ["success", "success", "product_error"],
  );
  assert.equal(result.rows[2].questions, 20);
  for (const metric of QUALITY_METRICS)
    assert.equal(result.rows[2].metrics[metric], 0);
  assert.equal(result.failed_tasks.length, 20);
  assert.ok(result.failed_tasks.every((task) => task.modes[0] === "vector"));
  assert.equal(result.rows[2].measurements.output_bytes_mean, null);
  assert.equal(result.rows[2].measurements.output_sample_count, 0);
  assert.equal(result.rows[2].measurements.latency_sample_count, 0);
  report.product_error_calls = 99;
  assertUnavailable(await buildCiSummary({ zg: report }));
});

test("an earlier repetition failure contributes zero without hiding the successful fifth call", async () => {
  const report = zgReport(),
    first = report.tasks[0].measurement_observations[0];
  Object.assign(first, {
    execution_status: "product_error",
    status: "product_error",
    latency_ms: null,
    visible_output_bytes: 50,
  });
  report.tasks[0].quality_observations[0].items = [];
  report.tasks[0].quality_mean = summarizeRepeatedQuality(
    report.tasks[0].quality_observations,
    report.tasks[0].ndcg.targets,
  );
  report.tasks[0].ranking_repeatable = null;
  report.product_error_calls = 1;
  report.integrity_passed = false;
  const result = await buildCiSummary({ zg: report });
  assert.equal(result.status, "failed");
  assert.equal(result.rows[0].status, "product_error");
  assert.equal(result.rows[0].metrics.file_hit_at_5, 0.99);
  assert.equal(result.rows[0].measurements.output_sample_count, 20);
  assert.equal(result.rows[0].measurements.latency_sample_count, 99);
});

test("required job failures fail the summary without changing valid measured values", async () => {
  const jobs = Object.fromEntries(
    ["authorize", "quality-contract", "package-candidate", "sweqa"].map(
      (name) => [name, { result: "success" }],
    ),
  );
  assert.equal(
    (await buildCiSummary({ zg: zgReport(), jobResults: jobs })).status,
    "success",
  );
  for (const name of Object.keys(jobs)) {
    const changed = clone(jobs);
    changed[name].result = "failure";
    const result = await buildCiSummary({
      zg: zgReport(),
      jobResults: changed,
    });
    assert.equal(result.status, "failed");
    assert.equal(result.rows[0].metrics.file_hit_at_5, 1);
    assert.ok(result.errors.some((error) => error.includes(name)));
  }
});

test("fractional results are stable across task ordering and do not mutate evidence", async () => {
  const ranks = [1, 3, 7, 10, null, 6, 9, 5, 2, 8];
  const report = zgReport({
    rankForTask: (task) => ranks[suite.lock.tasks.indexOf(task) % ranks.length],
  });
  const snapshot = clone(report),
    before = await buildCiSummary({ zg: report });
  report.tasks.reverse();
  report.expected_task_ids.reverse();
  const after = await buildCiSummary({ zg: report });
  assert.equal(before.status, "success");
  assert.deepEqual(after.rows, before.rows);
  report.tasks.reverse();
  report.expected_task_ids.reverse();
  assert.deepEqual(report, snapshot);
});

test("a changing Top 10 is reported separately from its five-call mean", async () => {
  const report = zgReport();
  const row = report.tasks[0];
  row.quality_observations[0].items = [];
  row.quality_mean = summarizeRepeatedQuality(
    row.quality_observations,
    row.ndcg.targets,
  );
  row.ranking_repeatable = false;
  const result = await buildCiSummary({ zg: report });
  assert.equal(result.status, "success");
  assert.equal(result.rows[0].metrics.file_hit_at_5, 0.99);
  assert.equal(result.rows[0].ranking_repeatable_cases, 19);
  const task = result.tasks.find(
    (item) => item.task_id === row.task_id && item.mode === row.mode,
  );
  assert.equal(task.hit_at_10_calls, 4);
  assert.equal(task.ranking_repeatable, false);
});

test("Markdown exposes three arms, five quality metrics, stability and latency", async () => {
  const result = await buildCiSummary({
    zg: zgReport(),
    harnessCommit: "a".repeat(40),
    candidateCommit: "b".repeat(40),
    candidateRef: "feature/rust-search",
  });
  const text = markdownCiSummary(result);
  assert.match(text, /Rust public MCP full presentation/);
  assert.match(
    text,
    /Each question\/mode averages its five call-level quality scores/,
  );
  assert.match(text, /Stable Top 10/);
  assert.match(text, /nDCG@10 averages repositories/);
  assert.match(text, /1 KiB = 1024 bytes/);
  assert.match(text, /20 output samples; 100 latency samples/);
  assert.match(text, /feature\/rust-search/);
  assert.match(text, new RegExp("a{40}"));
  assert.match(text, new RegExp("b{40}"));
  for (const label of ["zg-hybrid", "zg-fts", "zg-vector"])
    assert.match(
      text,
      new RegExp(
        `\\| ${label} \\| ✅ Valid \\| 20/20 questions; 11/11 repositories \\| 20/20 \\| 0.0% \\| 100.0% \\| 100.0% \\| 0.3333`,
      ),
    );
  assert.match(text, /\| 3\.50 \| 12\.35 \| 12\.35 \|/);
  assert.equal(
    text.split("\n").filter((line) => line.startsWith("| zg-")).length,
    3,
  );
  assert.doesNotMatch(
    text,
    /SDK|Disabled|comparison\.json|short|nDCG@5|anchor/,
  );
});

test("CLI accepts only --zg and --output and writes only the two overview artifacts", async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "zg-ci-summary-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const input = join(directory, "report.json"),
    output = join(directory, "summary");
  await writeFile(input, JSON.stringify(zgReport()));
  const script = fileURLToPath(new URL("../ci-report.mjs", import.meta.url));
  const env = { ...process.env, RETRIEVAL_JOB_RESULTS: "{}" };
  const result = spawnSync(
    process.execPath,
    [script, "--zg", input, "--output", output],
    { encoding: "utf8", env },
  );
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual((await readdir(output)).sort(), [
    "summary.json",
    "summary.md",
  ]);
  assert.equal(
    JSON.parse(await readFile(join(output, "summary.json"), "utf8"))
      .schema_version,
    5,
  );
  for (const unsupported of ["--engine", "--modes"])
    assert.notEqual(
      spawnSync(
        process.execPath,
        [script, "--zg", input, "--output", output, unsupported, "ignored"],
        { encoding: "utf8", env },
      ).status,
      0,
    );
});
