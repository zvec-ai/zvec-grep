import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";
import {
  scoreQaRetrieval,
  summarizeRepeatedQaRetrieval,
  summarizeQaRetrieval,
  validateQaPair,
  publicQaItems,
} from "../metrics/entities.mjs";
import { scoreFileRetrieval } from "../metrics/files.mjs";
import { parseVisibleResponse } from "../engines/zg/parse.mjs";

const sha = (value) => createHash("sha256").update(value).digest("hex");
const target = (id, path, symbol = id, start = 10) => ({
  id,
  path,
  symbol,
  file_target_id: `file-${id}`,
  kind: "function",
  file_sha256: "a".repeat(64),
  start_line: start,
  end_line: start + 2,
  declaration_start_line: start + 1,
  declaration_end_line: start + 1,
  declaration_text: `def ${symbol.split(".").at(-1)}():`,
  definition_text: `@decorator\ndef ${symbol.split(".").at(-1)}():\n    return 1\n`,
  definition_sha256: sha(
    `@decorator\ndef ${symbol.split(".").at(-1)}():\n    return 1\n`,
  ),
  anchors: [],
});
const a = target("answer", "src/a.py");
const b = target("helper", "src/b.py");
const pair = (entities = [a, b]) => ({
  query: "Where is the behavior implemented?",
  query_sha256: "q",
  repository: "owner/repo",
  repository_commit: "b".repeat(40),
  review: { correctness: "reviewed", completeness: "not_reviewed" },
  scoring_eligible: false,
  diagnostic_eligible: true,
  file_targets: entities.map((entity) => ({
    id: entity.file_target_id,
    path: entity.path,
    file_sha256: entity.file_sha256,
  })),
  entity_targets: entities,
});
const item = (entity, rank = 1, start = entity.start_line) => ({
  rank,
  path: entity.path,
  range: { kind: "text", start_line: start, end_line: entity.end_line },
  matched_by: "fts",
  metadata: [`symbol: [function] ${entity.symbol}`],
  source_lines: [
    { line: entity.declaration_start_line, text: entity.declaration_text },
  ],
});

test("QA diagnostic labels cannot produce formal totals without a completeness review", () => {
  const diagnostic = scoreQaRetrieval([item(a)], pair());
  assert.equal(diagnostic.status, "diagnostic");
  assert.equal(diagnostic.formal_eligible, false);
  const rows = [{ task_id: "q", qa_retrieval: diagnostic }];
  const summary = summarizeQaRetrieval(rows);
  assert.equal(summary.formal.eligible_tasks, 0);
  assert.equal(summary.formal.entity.mrr_at_10, null);
  assert.equal(summary.diagnostic.entity.recall_at_10, 0.5);
  const approved = pair();
  approved.review.completeness = "reviewed";
  approved.scoring_eligible = true;
  assert.equal(scoreQaRetrieval([item(a)], approved).status, "scored");
  const disputed = {
    ...pair(),
    diagnostic_eligible: false,
    review: { correctness: "disputed", completeness: "not_reviewed" },
  };
  assert.equal(scoreQaRetrieval([item(a)], disputed).entity, null);
  assert.equal(
    scoreQaRetrieval([item(a)], { ...pair(), scoring_eligible: true }).status,
    "diagnostic",
  );
});

test("Entity identity survives decorator boundary changes but not parser names or hashes alone", () => {
  const decorated = item(a);
  const defStart = item(a, 1, a.declaration_start_line);
  assert.equal(scoreQaRetrieval([decorated], pair()).entity.hit_at_1, 1);
  assert.equal(scoreQaRetrieval([defStart], pair()).entity.hit_at_1, 1);
  const body = item(a, 1, a.end_line);
  body.source_lines = [{ line: a.end_line, text: "    return 1" }];
  assert.equal(scoreQaRetrieval([body], pair()).entity.hit_at_1, 1);
  const fabricated = {
    ...item(a),
    source_lines: [
      { line: a.declaration_start_line, text: "def different():" },
    ],
  };
  assert.equal(scoreQaRetrieval([fabricated], pair()).entity.hit_at_1, null);
  assert.equal(
    scoreQaRetrieval([{ ...item(a), source_lines: [] }], pair()).entity
      .hit_at_1,
    null,
  );
});

test("parent ranges, overlapping bodies and same-name symbols cannot automatically claim child targets", () => {
  const child = target("method", "src/a.py", "Class.method", 10);
  const parent = {
    ...item(child),
    range: { kind: "text", start_line: 1, end_line: 200 },
    metadata: ["symbol: [class] Class"],
  };
  assert.equal(scoreQaRetrieval([parent], pair([child])).entity.hit_at_1, 0);
  assert.equal(
    scoreQaRetrieval([{ ...parent, metadata: [] }], pair([child])).entity
      .hit_at_1,
    null,
  );
  const wrongScope = {
    ...item(child),
    metadata: ["symbol: [method] method scope: OtherClass"],
  };
  assert.equal(
    scoreQaRetrieval([wrongScope], pair([child])).entity.hit_at_1,
    1,
  );
  const correctScope = {
    ...item(child),
    metadata: ["symbol: [method] method scope: Class"],
  };
  assert.equal(
    scoreQaRetrieval([correctScope], pair([child])).entity.hit_at_1,
    1,
  );
  const anonymousBody = {
    ...item(child, 1, child.end_line),
    metadata: [],
    source_lines: [{ line: child.end_line, text: "    return 1" }],
  };
  assert.equal(
    scoreQaRetrieval([anonymousBody], pair([child])).entity.hit_at_1,
    null,
  );
});

test("a visible declaration identifies its returned unit without public symbol metadata", () => {
  const visible = { ...item(a), metadata: [] };
  assert.equal(scoreQaRetrieval([visible], pair()).entity.hit_at_1, 1);
  assert.throws(
    () => scoreQaRetrieval([{ ...visible, rank: 2 }], pair()),
    /original consecutive/,
  );
  assert.throws(
    () =>
      scoreQaRetrieval(
        [{ ...visible, range: { kind: "text", start_line: 1, end_line: 2 } }],
        pair(),
      ),
    /outside the returned range/,
  );
  assert.throws(
    () =>
      scoreQaRetrieval(
        [{ ...visible, range: { kind: "text", start_line: 5, end_line: 4 } }],
        pair(),
      ),
    /invalid public source range/,
  );
});

test("verified source declarations survive renamed parser symbols without granting parent credit", () => {
  const method = target("method", "src/git.py", "Git.is_dirty", 10);
  method.kind = "method";
  const renamed = {
    ...item(method, 1, method.declaration_start_line),
    metadata: ["symbol: [method] parser_renamed_method"],
  };
  assert.equal(scoreQaRetrieval([renamed], pair([method])).entity.hit_at_1, 1);
  const conflict = { ...renamed, metadata: ["symbol: [class] Git"] };
  assert.equal(
    scoreQaRetrieval([conflict], pair([method])).entity.hit_at_1,
    null,
  );
  const aggregate = {
    ...conflict,
    range: { kind: "text", start_line: 1, end_line: 100 },
  };
  assert.equal(
    scoreQaRetrieval([aggregate], pair([method])).entity.hit_at_1,
    0,
  );
});

test("duplicate entities retain native positions and only first occurrences add Recall and nDCG credit", () => {
  const result = scoreQaRetrieval([item(a, 1), item(a, 2), item(b, 3)], pair());
  assert.deepEqual(result.entity.target_ranks, [1, 3]);
  assert.equal(result.entity.recall_at_1, 0.5);
  assert.equal(result.entity.recall_at_5, 1);
  assert.equal(result.entity.recall_at_10, 1);
  assert.ok(Math.abs(result.entity.ndcg_at_10 - 0.9197207891481876) < 1e-12);
  assert.equal(result.file.recall_at_1, 0.5);
  assert.equal(result.file.recall_at_5, 1);
});

test("new QA file matching is exact while legacy File metrics remain unchanged", () => {
  const suffix = { ...item(a), path: "a.py" };
  assert.equal(scoreQaRetrieval([suffix], pair()).file.hit_at_1, 0);
  assert.equal(
    scoreFileRetrieval([suffix], [{ path: "src/a.py" }]).hit_at_1,
    1,
  );
  assert.throws(
    () => scoreQaRetrieval([{ ...item(a), path: "/root/src/a.py" }], pair()),
    /relative/,
  );
});

test("unknown identity withholds only affected metrics and never silently becomes zero", () => {
  const unknown = { ...item(a), source_lines: [] };
  const result = scoreQaRetrieval([unknown, item(a, 2)], pair()).entity;
  assert.equal(result.hit_at_1, null);
  assert.equal(result.recall_at_1, null);
  assert.equal(result.hit_at_5, 1);
  assert.equal(result.recall_at_5, 0.5);
  assert.equal(result.rr_at_10, null);
  assert.equal(result.ndcg_at_10, null);
  assert.equal(scoreQaRetrieval([], pair()).entity.hit_at_10, 0);
  assert.equal(
    scoreQaRetrieval([], pair(), { executionStatus: "product_error" }).entity,
    null,
  );
  assert.equal(
    scoreQaRetrieval([], pair(), { status: "harness_invalid" }).reason,
    "harness_invalid",
  );
});

test("five repeated observations give a question one vote, with per-metric unknown counts", () => {
  const calls = [1, 2, 3, 4, 5].map((repetition) => ({
    repetition,
    status: "scored",
    execution_status: "success",
    items: [item(a)],
  }));
  calls[0].items[0].source_lines = [];
  const mean = summarizeRepeatedQaRetrieval(calls, pair());
  assert.equal(mean.entity.recall_at_10, null);
  assert.equal(mean.entity.scored_calls.recall_at_10, 4);
  assert.equal(mean.file.recall_at_10, 0.5);
  const summary = summarizeQaRetrieval([
    { task_id: "unknown", qa_quality_mean: mean },
    {
      task_id: "known",
      qa_retrieval: scoreQaRetrieval([item(a), item(b, 2)], pair()),
    },
    { task_id: "invalid", status: "harness_invalid" },
  ]);
  assert.equal(summary.planned_tasks, 3);
  assert.equal(summary.diagnostic.eligible_tasks, 2);
  assert.equal(summary.diagnostic.entity.recall_at_10, 1);
  assert.equal(summary.diagnostic.entity.metric_task_counts.recall_at_10, 1);
  assert.equal(summary.diagnostic.file.recall_at_10, 0.75);
  assert.equal(summary.unavailable_tasks[0].task_id, "invalid");
});

test("real compact public output retains enough evidence for QA scoring and replay", () => {
  const response = {
    content: [
      {
        type: "text",
        text: "freshness: fresh\n#1 src/a.py:11-12\nsymbol: [function] answer\n  11: def answer():\n  12:     return 1\n",
      },
    ],
  };
  const parsed = parseVisibleResponse(response);
  const scored = scoreQaRetrieval(parsed.items, pair());
  assert.equal(scored.entity.hit_at_1, 1);
  assert.deepEqual(
    scoreQaRetrieval(publicQaItems(parsed.items), pair()),
    scored,
  );
  assert.equal(publicQaItems(parsed.items)[0].matched_by, null);
});

test("QA validation binds original query and fixed source text, refusing false formal eligibility", () => {
  const qa = pair();
  const task = { ...qa, task_id: "q" };
  validateQaPair(task, qa);
  assert.throws(
    () => validateQaPair(task, { ...qa, query: "rewritten" }),
    /query changed/,
  );
  assert.throws(
    () => validateQaPair(task, { ...qa, scoring_eligible: true }),
    /reviewed/,
  );
  const altered = structuredClone(qa);
  altered.entity_targets[0].definition_text += "# changed";
  assert.throws(
    () => validateQaPair(task, altered),
    /definition content changed/,
  );
});

test("different annotation IDs cannot count one source declaration twice", () => {
  const qa = pair([a]);
  const task = { ...qa, task_id: "q" };
  const duplicate = structuredClone(qa);
  duplicate.entity_targets.push({
    ...duplicate.entity_targets[0],
    id: "different-annotation-id",
  });
  assert.throws(
    () => validateQaPair(task, duplicate),
    /duplicate QA entity declaration/,
  );

  const overload = target("second-declaration", a.path, a.symbol, 40);
  overload.file_target_id = a.file_target_id;
  qa.entity_targets.push(overload);
  assert.doesNotThrow(() => validateQaPair(task, qa));
});
