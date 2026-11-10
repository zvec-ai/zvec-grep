import assert from "node:assert/strict";
import { validateSearchRoute } from "../engines/zg/parse.mjs";
import { scoreNdcg } from "../metrics/ndcg.mjs";
import {
  FILE_RETRIEVAL_CONTRACT,
  fileRetrievalForRow,
} from "../metrics/files.mjs";
import { validateCallLatency } from "../metrics/measurements.mjs";
import {
  rankedLocations,
  rankingSignature,
  summarizeRepeatedQuality,
} from "../metrics/repetitions.mjs";
import {
  publicQaItems,
  scoreQaRetrieval,
  summarizeRepeatedQaRetrieval,
  summarizeQaRetrieval,
} from "../metrics/entities.mjs";

export const ZG_MODES = Object.freeze(["hybrid", "fts", "vector"]);
const FILE_METRICS = ["hit_at_1", "hit_at_5", "hit_at_10", "rr_at_10"];
const CATEGORIES = ["what", "where", "how", "why"];
const key = (row) => JSON.stringify([row.task_id, row.mode]);

function validateQaQualityRow(row, pair, label) {
  assert.ok(pair, `${label}: missing frozen QA pair`);
  assert.ok(
    Array.isArray(row.qa_observations) && row.qa_observations.length === 5,
    `${label}: incomplete QA repetition evidence`,
  );
  for (const [index, observation] of row.qa_observations.entries()) {
    const measurement = row.measurement_observations[index];
    assert.equal(observation.repetition, index + 1);
    for (const field of ["status", "execution_status"])
      assert.equal(
        observation[field],
        measurement[field],
        `${label}: QA ${field} differs from captured call`,
      );
    assert.deepEqual(
      rankedLocations(observation.items),
      row.quality_observations[index].items,
      `${label}: QA locations differ from public quality evidence`,
    );
  }
  assert.deepEqual(
    row.qa_observations[4].items,
    publicQaItems(row.items),
    `${label}: fifth-call QA evidence differs from representative response`,
  );
  assert.deepEqual(
    row.qa_retrieval,
    scoreQaRetrieval(row.items, pair, {
      executionStatus: row.execution_status,
      status: row.status,
    }),
    `${label}: QA scores differ from public evidence or frozen Gold`,
  );
  assert.deepEqual(
    row.qa_quality_mean,
    summarizeRepeatedQaRetrieval(row.qa_observations, pair),
    `${label}: QA repeated scores differ from public evidence`,
  );
}

function uniqueStrings(values, label) {
  assert.ok(
    Array.isArray(values) && values.length > 0,
    `${label}: missing values`,
  );
  assert.ok(
    values.every((value) => typeof value === "string" && value.length > 0),
    `${label}: invalid value`,
  );
  assert.equal(
    new Set(values).size,
    values.length,
    `${label}: duplicate value`,
  );
  return [...values].sort();
}

/** Bind row identity and labels to the checked-out frozen suite. */
export function validateFrozenTask(
  row,
  suite,
  label,
  { allowMissingTargets = false } = {},
) {
  const task = suite.lock.tasks.find((task) => task.task_id === row.task_id);
  assert.ok(task, `${label}: unknown task ${row.task_id}`);
  for (const field of ["repository", "category"])
    assert.equal(row[field], task[field], `${label}: ${field} mismatch`);
  assert.equal(
    row.language,
    suite.file_gold[row.task_id].language,
    `${label}: language mismatch`,
  );
  assert.equal(
    row.gold_status,
    suite.gold[row.task_id].status,
    `${label}: Gold status mismatch`,
  );
  if (!allowMissingTargets)
    assert.deepEqual(
      row.ndcg?.targets,
      suite.file_gold[row.task_id].targets,
      `${label}: frozen file targets differ`,
    );
}

/** Check all five calls before aggregation can omit invalid measurements. */
function validateMeasurements(row, label) {
  const samples = row.measurement_observations;
  assert.ok(
    Array.isArray(samples) && samples.length === 5,
    `${label}: incomplete measurement repetition coverage`,
  );
  const repetitions = samples.map((sample) => sample.repetition);
  assert.deepEqual(
    repetitions.sort((a, b) => a - b),
    [1, 2, 3, 4, 5],
    `${label}: invalid measurement repetition coverage`,
  );
  for (const sample of samples) {
    assert.ok(
      ["success", "product_error"].includes(sample.execution_status),
      `${label}: invalid measurement execution status`,
    );
    assert.equal(
      sample.status,
      row.gold_status === "reviewed"
        ? sample.execution_status === "success"
          ? "scored"
          : "product_error"
        : `gold_${row.gold_status}`,
      `${label}: inconsistent measurement scoring status`,
    );
    try {
      validateCallLatency(sample);
    } catch (error) {
      throw new Error(`${label}: measurement ${error.message}`, {
        cause: error,
      });
    }
    if (sample.execution_status === "success") {
      assert.ok(
        Number.isSafeInteger(sample.visible_output_bytes) &&
          sample.visible_output_bytes >= 0,
        `${label}: invalid successful-call output bytes`,
      );
    }
  }
  const quality = samples.find((sample) => sample.repetition === 5);
  for (const field of [
    "status",
    "execution_status",
    "latency_ms",
    "visible_output_bytes",
  ])
    assert.equal(
      quality[field],
      row[field],
      `${label}: quality measurement differs from quality row`,
    );
}

/** Recompute quality from native public items and verify saved row evidence. */
function validateQualityRow(row, label) {
  const targets = row.ndcg?.targets;
  assert.ok(
    Array.isArray(targets) && targets.length > 0,
    `${label}: missing frozen file-target projection`,
  );
  assert.ok(
    targets.every(
      (target) =>
        Object.keys(target).length === 1 &&
        typeof target.path === "string" &&
        target.path.length > 0,
    ),
    `${label}: expected projected file-only targets`,
  );
  assert.equal(
    new Set(targets.map((target) => target.path)).size,
    targets.length,
    `${label}: duplicate projected file targets`,
  );
  assert.ok(Array.isArray(row.items), `${label}: missing public parsed items`);
  assert.ok(
    ["success", "product_error"].includes(row.execution_status),
    `${label}: invalid execution status`,
  );
  if (row.execution_status === "product_error")
    assert.equal(
      row.items.length,
      0,
      `${label}: product errors cannot provide retrieval credit`,
    );
  if (row.gold_status === "reviewed") {
    assert.equal(
      row.status,
      row.execution_status === "success" ? "scored" : "product_error",
      `${label}: inconsistent scored status`,
    );
  } else {
    assert.ok(
      ["unknown", "disputed"].includes(row.gold_status),
      `${label}: invalid gold status`,
    );
    assert.equal(
      row.status,
      `gold_${row.gold_status}`,
      `${label}: inconsistent unscored status`,
    );
  }
  validateSearchRoute(row.items, row.mode);
  const ndcg = { targets, ...scoreNdcg(row.items, targets) };
  const saved = { ...row.ndcg };
  assert.deepEqual(
    saved,
    ndcg,
    `${label}: nDCG score differs from public items or frozen projection`,
  );
  for (const field of [
    "first_hit_rank",
    ...FILE_METRICS,
    "ndcg_at_5",
    "ndcg_at_10",
    "target_matches",
    "repeat_ranks",
    "stage_evidence",
  ])
    assert.ok(
      !Object.hasOwn(row, field),
      `${label}: obsolete report field ${field}`,
    );
  const normalized = { ...row, ndcg };
  const file = fileRetrievalForRow(normalized);
  assert.deepEqual(
    row.file_retrieval,
    file,
    `${label}: file retrieval score differs from public items or frozen projection`,
  );
  validateMeasurements(row, label);
  assert.ok(
    Array.isArray(row.quality_observations) &&
      row.quality_observations.length === 5,
    `${label}: incomplete quality repetition coverage`,
  );
  for (const [index, observation] of row.quality_observations.entries()) {
    assert.equal(observation.repetition, index + 1);
    assert.ok(Array.isArray(observation.items));
    const measurement = row.measurement_observations[index];
    if (measurement.execution_status === "product_error")
      assert.deepEqual(
        observation.items,
        [],
        `${label}: failed call earned credit`,
      );
    else validateSearchRoute(observation.items, row.mode);
  }
  assert.equal(
    rankingSignature(row.quality_observations[4].items),
    rankingSignature(row.items),
    `${label}: fifth quality evidence differs from representative row`,
  );
  const repeated =
    row.gold_status === "reviewed"
      ? summarizeRepeatedQuality(row.quality_observations, targets)
      : null;
  assert.deepEqual(
    row.quality_mean,
    repeated,
    `${label}: five-call quality mean or stability differs from public items`,
  );
  assert.equal(
    row.ranking_repeatable,
    row.measurement_observations.every(
      (sample) => sample.execution_status === "success",
    )
      ? (repeated?.ranking_repeatable ?? null)
      : null,
    `${label}: ranking stability differs from public items`,
  );
  return { ...normalized, file_retrieval: file };
}

/** Validate the Rust MCP-full, three-mode report from saved public evidence. */
export function validateZgReport(
  report,
  label = "ZG",
  { suite, allowPartial = false } = {},
) {
  const hasQa =
    Object.hasOwn(report, "qa_gold") ||
    report.tasks?.some((row) =>
      ["qa_retrieval", "qa_quality_mean", "qa_observations"].some((field) =>
        Object.hasOwn(row, field),
      ),
    ) ||
    Object.values(report.modes ?? {}).some((mode) =>
      Object.hasOwn(mode, "qa_retrieval"),
    );
  if (hasQa) {
    assert.ok(
      suite?.qa_pairs && suite?.qa_gold,
      `${label}: QA validation requires the frozen QA suite`,
    );
    assert.deepEqual(
      report.qa_gold,
      suite.qa_gold,
      `${label}: QA Gold identity mismatch`,
    );
  }
  assert.equal(
    report.schema_version,
    7,
    `${label}: unsupported report schema; schema 7 required`,
  );
  assert.equal(
    report.preview,
    "full",
    `${label}: Rust MCP full presentation required`,
  );
  for (const field of [
    "previews",
    "primary_preview",
    "paired_preview_comparison",
    "preview_pairs",
  ])
    assert.ok(
      !Object.hasOwn(report, field),
      `${label}: obsolete preview field ${field}`,
    );
  assert.equal(
    report.file_retrieval_contract,
    FILE_RETRIEVAL_CONTRACT,
    `${label}: incompatible file retrieval contract`,
  );
  assert.equal(
    report.quality_repetition,
    5,
    `${label}: quality repetition 5 required`,
  );
  assert.equal(
    report.quality_aggregation,
    "mean_of_five",
    `${label}: five-call quality aggregation required`,
  );
  const partial = allowPartial && report.quality_score_valid === false;
  if (partial) {
    // Only isolated, explicitly marked observations may be omitted. Global
    // protocol, candidate, corpus, or model failures still withhold all scores.
    assert.deepEqual(
      report.integrity_errors,
      ["one or more observations are experimentally invalid"],
      `${label}: non-local experimental integrity errors`,
    );
    assert.equal(report.integrity_passed, false);
  } else {
    assert.equal(
      report.quality_score_valid,
      true,
      `${label}: invalid quality report`,
    );
    assert.ok(
      Array.isArray(report.integrity_errors) &&
        report.integrity_errors.length === 0,
      `${label}: experimental integrity errors`,
    );
  }
  assert.ok(
    Number.isSafeInteger(report.product_error_calls) &&
      report.product_error_calls >= 0,
    `${label}: invalid product error count`,
  );
  if (!partial)
    assert.equal(
      report.integrity_passed,
      report.product_error_calls === 0,
      `${label}: integrity flag and product errors disagree`,
    );
  for (const field of ["source", "gold", "file_gold", "protocol"])
    assert.match(
      report.suite?.[field] ?? "",
      /^[a-f0-9]{64}$/,
      `${label}: missing suite ${field} identity`,
    );
  if (suite)
    assert.deepEqual(
      report.suite,
      suite.identity,
      `${label}: frozen suite identity mismatch`,
    );

  const ids = uniqueStrings(
    report.expected_task_ids,
    `${label}: expected tasks`,
  );
  assert.ok(
    ["full-20-original-queries", "explicit-subset"].includes(report.scope),
    `${label}: unknown scope`,
  );
  assert.ok(
    report.scope === "full-20-original-queries"
      ? ids.length === 20
      : ids.length < 20,
    `${label}: scope/task-count mismatch`,
  );
  assert.ok(
    report.modes &&
      typeof report.modes === "object" &&
      !Array.isArray(report.modes),
    `${label}: missing modes`,
  );
  const modes = uniqueStrings(Object.keys(report.modes), `${label}: modes`);
  assert.deepEqual(
    modes,
    [...ZG_MODES].sort(),
    `${label}: all three modes are required`,
  );
  assert.equal(
    report.observed_calls,
    ids.length * ZG_MODES.length * 5,
    `${label}: incomplete call matrix`,
  );
  assert.ok(Array.isArray(report.tasks), `${label}: missing task rows`);
  const rows = new Map();
  const invalidRows = [];
  const seen = new Set();
  for (const row of report.tasks) {
    const context = `${label}: ${row.task_id}/${row.mode}`;
    assert.equal(
      row.preview,
      "full",
      `${context}: Rust MCP full presentation required`,
    );
    assert.ok(
      ids.includes(row.task_id) && modes.includes(row.mode),
      `${context}: unexpected task/mode`,
    );
    assert.ok(!seen.has(key(row)), `${context}: duplicate task/mode`);
    seen.add(key(row));
    assert.equal(
      row.repetition,
      5,
      `${context}: quality repetition 5 required`,
    );
    assert.equal(
      row.quality_observation,
      true,
      `${context}: not a quality observation`,
    );
    assert.ok(
      CATEGORIES.includes(row.category),
      `${context}: invalid category`,
    );
    for (const field of ["repository", "language"])
      assert.ok(
        typeof row[field] === "string" && row[field].length > 0,
        `${context}: missing ${field}`,
      );
    const invalid = partial && row.status === "harness_invalid";
    if (suite)
      validateFrozenTask(row, suite, context, {
        allowMissingTargets: invalid,
      });
    if (invalid) {
      if (hasQa) {
        assert.equal(
          row.qa_retrieval,
          null,
          `${context}: invalid row has QA scores`,
        );
        assert.equal(
          row.qa_quality_mean,
          null,
          `${context}: invalid row has repeated QA scores`,
        );
      }
      assert.equal(
        row.file_retrieval,
        null,
        `${context}: invalid row has score`,
      );
      assert.equal(row.ndcg, null, `${context}: invalid row has nDCG`);
      assert.ok(
        typeof row.invalid_reason === "string" && row.invalid_reason.length,
        `${context}: missing invalid reason`,
      );
      assert.match(
        row.invalid_reason,
        /^(?:repetition [1-5]: )?format_unknown: [^;]+$/,
        `${context}: only isolated public-response format failures permit partial scoring`,
      );
      invalidRows.push(row);
    } else {
      if (hasQa)
        validateQaQualityRow(row, suite.qa_pairs[row.task_id], context);
      rows.set(key(row), validateQualityRow(row, context));
    }
  }
  assert.equal(
    seen.size,
    ids.length * ZG_MODES.length,
    `${label}: missing task/mode coverage`,
  );
  if (partial)
    assert.ok(invalidRows.length > 0, `${label}: no isolated invalid rows`);
  assert.equal(
    report.product_error_calls,
    report.tasks
      .flatMap((row) => row.measurement_observations)
      .filter((sample) => sample.execution_status === "product_error").length,
    `${label}: product error count differs from measurement evidence`,
  );
  if (hasQa)
    for (const mode of ZG_MODES) {
      const expected =
        report.quality_score_valid || partial
          ? summarizeQaRetrieval(
              report.tasks.filter((row) => row.mode === mode),
            )
          : null;
      assert.deepEqual(
        report.modes[mode].qa_retrieval,
        expected,
        `${label}: ${mode} QA aggregate differs from scored cases`,
      );
    }
  return { ids, modes: [...ZG_MODES], rows, invalidRows };
}
