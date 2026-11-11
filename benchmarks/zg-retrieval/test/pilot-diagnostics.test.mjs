import assert from "node:assert/strict";
import test from "node:test";
import { summarizePilotEntity } from "../expansion/gold.mjs";
import {
  summarizeDiagnosticRetrieval,
  summarizeQaRetrieval,
  summarizeRepeatedQaRetrieval,
} from "../metrics/entities.mjs";

const entry = {
  files: [{ path: "docs/a.md" }, { path: "docs/b.md" }],
  targets: [
    {
      source_id: "a",
      path: "docs/a.md",
      start_line: 1,
      end_line: 3,
      start_byte: 0,
      end_byte: 30,
    },
  ],
  unresolved: [{ source_id: "b", reason: "No supported answer span" }],
};
const labels = {
  file_targets: entry.files.map((file) => ({ ...file, id: file.path })),
  entity_targets: entry.targets.map((target) => ({
    ...target,
    id: target.source_id,
  })),
};
const calls = () =>
  [1, 2, 3, 4, 5].map((repetition) => ({
    repetition,
    status: "success",
    items: [
      {
        rank: 1,
        path: "docs/a.md",
        range: { kind: "text", start_line: 1, end_line: 3 },
        source_bytes: { start_byte: 0, end_byte: 30 },
      },
    ],
  }));
const reviewedPair = (review, extra = {}) => ({
  ...labels,
  review,
  diagnostic_eligible: true,
  scoring_eligible: false,
  ...extra,
});
const diagnosticReview = {
  correctness: "reviewed",
  completeness: "not_reviewed",
};
const byteOptions = { entityByteRanges: true };

test("Pilot labels score without claiming review and never produce formal totals", () => {
  const score = summarizePilotEntity(calls(), entry);
  assert.equal(score.status, "diagnostic");
  assert.equal(score.reason, "diagnostic_only");
  assert.equal(score.formal_eligible, false);
  assert.equal(score.file.n_relevant, 2);
  assert.equal(score.file.recall_at_10, 0.5);
  assert.equal(score.entity.n_relevant, 1);
  assert.equal(score.entity.recall_at_10, 1);
  assert.equal(score.entity.scored_calls.recall_at_10, 5);

  const shouldNotRead = {
    get review() {
      assert.fail("Pilot scoring must not read a QA review");
    },
    get scoring_eligible() {
      assert.fail("Pilot scoring must not read formal eligibility");
    },
    get diagnostic_eligible() {
      assert.fail("Pilot scoring must not read diagnostic eligibility");
    },
  };
  Object.defineProperties(
    shouldNotRead,
    Object.getOwnPropertyDescriptors(labels),
  );
  assert.deepEqual(
    summarizeDiagnosticRetrieval(calls(), shouldNotRead, byteOptions),
    score,
  );
  assert.deepEqual(
    summarizeDiagnosticRetrieval(
      calls(),
      reviewedPair(
        { correctness: "reviewed", completeness: "reviewed" },
        { scoring_eligible: true },
      ),
      byteOptions,
    ),
    score,
  );
  const aggregate = summarizeQaRetrieval([
    { task_id: "pilot", qa_quality_mean: score },
  ]);
  assert.equal(aggregate.formal.eligible_tasks, 0);
  assert.equal(aggregate.formal.entity.recall_at_10, null);
  assert.equal(aggregate.diagnostic.entity.metric_task_counts.recall_at_10, 1);
});

test("Pilot unresolved-only questions remain unscored, not zero", () => {
  const score = summarizePilotEntity([], {
    ...entry,
    targets: [],
    unresolved: [
      { source_id: "a", reason: "No supported answer span" },
      ...entry.unresolved,
    ],
  });
  assert.deepEqual(score, {
    status: "unavailable",
    formal_eligible: false,
    reason: "no_verified_entity_evidence",
    file: null,
    entity: null,
  });
  const aggregate = summarizeQaRetrieval([
    { task_id: "unresolved", qa_quality_mean: score },
  ]);
  assert.equal(aggregate.diagnostic.eligible_tasks, 0);
  assert.equal(aggregate.diagnostic.entity.metric_task_counts.recall_at_10, 0);
});

test("diagnostic math preserves five-call unknown and failure handling", () => {
  for (const change of [
    (call) => delete call.items[0].source_bytes,
    (call) => (call.execution_status = "product_error"),
    (call) => (call.status = "harness_invalid"),
  ]) {
    const observations = calls();
    change(observations[0]);
    const diagnostic = summarizePilotEntity(observations, entry);
    const qa = summarizeRepeatedQaRetrieval(
      observations,
      reviewedPair(diagnosticReview),
      byteOptions,
    );
    assert.deepEqual(diagnostic.file, qa.file);
    assert.deepEqual(diagnostic.entity, qa.entity);
    assert.equal(diagnostic.status, "diagnostic");
    assert.equal(diagnostic.formal_eligible, false);
    assert.equal(diagnostic.entity.recall_at_10, null);
    assert.equal(diagnostic.entity.scored_calls.recall_at_10, 4);
    const aggregate = summarizeQaRetrieval([
      { task_id: "unknown", qa_quality_mean: diagnostic },
    ]);
    assert.equal(
      aggregate.diagnostic.entity.metric_task_counts.recall_at_10,
      0,
    );
  }
  assert.throws(
    () => summarizePilotEntity(calls().slice(1), entry),
    /five calls/,
  );
  assert.throws(() => summarizePilotEntity(calls().reverse(), entry));
});

test("SWE QA still requires its real correctness and completeness eligibility", () => {
  const diagnostic = summarizeRepeatedQaRetrieval(
    calls(),
    reviewedPair(diagnosticReview),
    byteOptions,
  );
  assert.equal(diagnostic.status, "diagnostic");
  assert.equal(diagnostic.reason, "gold_completeness_not_reviewed");
  for (const correctness of ["disputed", undefined]) {
    const score = summarizeRepeatedQaRetrieval(
      calls(),
      reviewedPair(
        { correctness, completeness: "reviewed" },
        { scoring_eligible: true },
      ),
      byteOptions,
    );
    assert.equal(score.status, "unavailable");
    assert.equal(score.entity, null);
  }
  const formal = summarizeRepeatedQaRetrieval(
    calls(),
    reviewedPair(
      { correctness: "reviewed", completeness: "reviewed" },
      { scoring_eligible: true },
    ),
    byteOptions,
  );
  assert.equal(formal.status, "scored");
  assert.equal(formal.formal_eligible, true);
  assert.deepEqual(formal.entity, diagnostic.entity);
});
