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
  file_target_id: `file-${path}`,
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
  file_targets: [
    ...new Map(
      entities.map((entity) => [
        entity.path,
        {
          id: entity.file_target_id,
          path: entity.path,
          file_sha256: entity.file_sha256,
        },
      ]),
    ).values(),
  ],
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

test("Entity recall requires the entire Gold range, including its decorator prefix", () => {
  const complete = item(a);
  assert.equal(scoreQaRetrieval([complete], pair()).entity.hit_at_1, 1);
  for (const [start, end] of [
    [a.declaration_start_line, a.end_line],
    [a.start_line, a.end_line - 1],
    [a.end_line, a.end_line],
    [a.end_line + 1, a.end_line + 2],
  ]) {
    const partial = {
      ...complete,
      range: { kind: "text", start_line: start, end_line: end },
    };
    const score = scoreQaRetrieval([partial], pair()).entity;
    assert.equal(score.hit_at_1, 0);
    assert.equal(score.recall_at_10, 0);
    assert.deepEqual(score.unknown_ranks, []);
  }
});

test("one returned class can contain several Gold methods without creating extra ranks", () => {
  const first = target("first", "src/class.py", "Class.first", 10);
  const second = target("second", "src/class.py", "Class.second", 20);
  const qa = pair([first, second, b]);
  const parent = {
    ...item(first),
    range: { kind: "text", start_line: 1, end_line: 30 },
    metadata: ["symbol: [class] Class"],
    source_lines: [],
  };
  const score = scoreQaRetrieval([parent, item(b, 2)], qa);
  assert.deepEqual(score.entity.target_ranks, [1, 1, 2]);
  assert.equal(score.entity.recall_at_1, 2 / 3);
  assert.equal(score.entity.recall_at_5, 1);
  assert.equal(score.entity.ndcg_at_10, 1);
  assert.equal(score.file.recall_at_1, 0.5);
  assert.equal(score.file.ndcg_at_10, 1);
  assert.equal(
    scoreQaRetrieval([parent], pair([first, second])).entity.ndcg_at_10,
    1,
  );
});

test("preview text, indentation and parser names do not change Entity range containment", () => {
  const original = scoreQaRetrieval([item(a)], pair()).entity;
  for (const presentation of [
    { metadata: [], source_lines: [] },
    { metadata: ["symbol: [class] Renamed"], source_lines: [] },
    {
      metadata: ["symbol: [method] other scope: OtherClass"],
      source_lines: [
        { line: a.declaration_start_line, text: "    def answer():" },
      ],
    },
    {
      metadata: [],
      source_lines: [
        { line: a.declaration_start_line, text: "def different():" },
      ],
    },
    {
      metadata: [],
      source_lines: [{ line: 1, text: "unrelated preview context" }],
    },
  ]) {
    assert.deepEqual(
      scoreQaRetrieval([{ ...item(a), ...presentation }], pair()).entity,
      original,
    );
  }
  assert.throws(
    () => scoreQaRetrieval([{ ...item(a), rank: 2 }], pair()),
    /original consecutive/,
  );
  assert.throws(
    () =>
      scoreQaRetrieval(
        [{ ...item(a), range: { kind: "text", start_line: 5, end_line: 4 } }],
        pair(),
      ),
    /invalid public source range/,
  );
});

test("Entity ranges only cover Gold in the same repository-relative file", () => {
  const broad = {
    ...item(a),
    range: { kind: "text", start_line: 1, end_line: 1000 },
  };
  const score = scoreQaRetrieval([broad], pair()).entity;
  assert.deepEqual(score.target_ranks, [1, null]);
  assert.equal(score.recall_at_10, 0.5);
  assert.equal(
    scoreQaRetrieval([{ ...broad, path: "a.py" }], pair()).entity.hit_at_10,
    0,
  );
});

test("single-line Gold at an Entity boundary stays unknown without source columns", () => {
  const single = { ...a, start_line: 11, end_line: 11 };
  for (const [start, end] of [
    [11, 11],
    [11, 12],
    [10, 11],
  ]) {
    const score = scoreQaRetrieval(
      [
        {
          ...item(single),
          range: { kind: "text", start_line: start, end_line: end },
        },
      ],
      pair([single]),
    ).entity;
    assert.equal(score.hit_at_1, null);
    assert.equal(score.recall_at_10, null);
    assert.deepEqual(score.unknown_ranks, [
      { rank: 1, target_ids: [single.id] },
    ]);
  }
  const enclosing = {
    ...item(single),
    range: { kind: "text", start_line: 10, end_line: 12 },
  };
  assert.equal(
    scoreQaRetrieval([enclosing], pair([single])).entity.recall_at_1,
    1,
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

test("duplicate parent Entities keep their positions without adding multi-target credit", () => {
  const second = target("second", a.path, "Class.second", 20);
  const parent = {
    ...item(a),
    range: { kind: "text", start_line: 1, end_line: 30 },
  };
  const score = scoreQaRetrieval(
    [parent, { ...parent, rank: 2 }, item(b, 3)],
    pair([a, second, b]),
  ).entity;
  assert.deepEqual(score.target_ranks, [1, 1, 3]);
  assert.equal(score.recall_at_1, 2 / 3);
  assert.equal(score.recall_at_10, 1);
  const expected = (2 + 1 / Math.log2(4)) / (2 + 1 / Math.log2(3));
  assert.ok(Math.abs(score.ndcg_at_10 - expected) < 1e-12);
  assert.ok(score.ndcg_at_10 < 1);
});

test("Entity nDCG uses fixed per-file ideal gains and remains bounded with multiple Gold per rank", () => {
  const sameFile = [
    a,
    target("second", a.path, "second", 20),
    target("third", a.path, "third", 30),
  ];
  const qa = pair([...sameFile, b]);
  const parent = {
    ...item(a),
    range: { kind: "text", start_line: 1, end_line: 40 },
  };
  const perfect = scoreQaRetrieval([parent, item(b, 2)], qa).entity;
  assert.equal(perfect.ndcg_at_10, 1);
  const laterParent = scoreQaRetrieval(
    [item(b), { ...parent, rank: 2 }],
    qa,
  ).entity;
  const expected = (1 + 3 / Math.log2(3)) / (3 + 1 / Math.log2(3));
  assert.ok(Math.abs(laterParent.ndcg_at_10 - expected) < 1e-12);
  const split = scoreQaRetrieval(
    [...sameFile.map((gold, index) => item(gold, index + 1)), item(b, 4)],
    qa,
  ).entity;
  assert.equal(split.recall_at_10, 1);
  assert.ok(split.ndcg_at_10 > 0 && split.ndcg_at_10 < 1);

  const many = Array.from({ length: 12 }, (_, index) =>
    target(`target-${index}`, a.path, `target${index}`, 10 + index * 5),
  );
  const complete = scoreQaRetrieval(
    [{ ...parent, range: { kind: "text", start_line: 1, end_line: 100 } }],
    pair(many),
  ).entity;
  assert.equal(complete.recall_at_1, 1);
  assert.equal(complete.ndcg_at_10, 1);
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

test("missing Entity ranges withhold only affected metrics and never silently become zero", () => {
  const unknown = { ...item(a), range: undefined };
  const result = scoreQaRetrieval([unknown, item(a, 2)], pair()).entity;
  assert.equal(result.hit_at_1, null);
  assert.equal(result.recall_at_1, null);
  assert.equal(result.hit_at_5, 1);
  assert.equal(result.recall_at_5, 0.5);
  assert.equal(result.rr_at_10, null);
  assert.equal(result.ndcg_at_10, null);
  for (const range of [undefined, null, { kind: "other", label: "file" }]) {
    const unlocated = { ...item(a), range };
    const score = scoreQaRetrieval([unlocated], pair()).entity;
    assert.deepEqual(score.unknown_ranks, [{ rank: 1, target_ids: [a.id] }]);
    assert.equal(score.recall_at_10, null);
    assert.equal(
      scoreQaRetrieval([{ ...unlocated, path: "unrelated.py" }], pair()).entity
        .recall_at_10,
      0,
    );
  }
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
  calls[0].items[0].range = undefined;
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
        text: "freshness: fresh\n#1 src/a.py:10-12\nsymbol: [function] answer\n  11: def answer():\n  12:     return 1\n",
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
