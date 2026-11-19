import assert from "node:assert/strict";
import test from "node:test";
import {
  scoreQaRetrieval,
  summarizeRepeatedQaRetrieval,
} from "../metrics/entities.mjs";

const path = "docs/article.md";
const span = (line) => ({
  start_line: line,
  end_line: line,
  start_byte: (line - 1) * 100 + 10,
  end_byte: (line - 1) * 100 + 90,
});
const target = (...alternatives) => ({
  id: "article",
  path,
  evidence_sets: alternatives.map((spans, index) => ({
    id: `set-${index}`,
    spans,
  })),
});
const pair = (targets) => ({
  diagnostic_eligible: true,
  scoring_eligible: false,
  review: { correctness: "reviewed", completeness: "not_reviewed" },
  file_targets: [...new Set(targets.map((target) => target.path))].map(
    (path) => ({ id: path, path }),
  ),
  entity_targets: targets,
});
const item = (range, extra = {}) => ({
  path,
  range: {
    kind: "text",
    start_line: range.start_line,
    end_line: range.end_line,
  },
  source_bytes: { start_byte: range.start_byte, end_byte: range.end_byte },
  ...extra,
});
const ranked = (items) =>
  items.map((item, index) => ({ ...item, rank: index + 1 }));
const score = (items, targets) =>
  scoreQaRetrieval(ranked(items), pair(targets), {
    entityByteRanges: true,
  });
const filler = () => item(span(1), { path: "docs/unrelated.md" });

test("either complete alternative hits one logical target without requiring intervening sentences", () => {
  const gold = target([span(2)], [span(7), span(9)]);
  assert.equal(score([item(span(2))], [gold]).entity.recall_at_1, 1);
  const result = score([item(span(7)), item(span(9))], [gold]);
  assert.deepEqual(result.entity.target_ranks, [2]);
  assert.equal(result.entity.n_relevant, 1);
  assert.equal(result.entity.hit_at_1, 0);
  assert.equal(result.entity.recall_at_5, 1);
  assert.equal(result.entity.rr_at_10, 0.5);
  assert.equal(result.entity.ndcg_at_10, 1 / Math.log2(3));
  assert.equal(result.file.recall_at_1, 1);
});

test("all spans within one alternative are required, and alternatives cannot supply each other's missing members", () => {
  const gold = target([span(2), span(3)], [span(7), span(9)]);
  for (const items of [[item(span(7))], [item(span(2)), item(span(9))]]) {
    const result = score(items, [gold]).entity;
    assert.deepEqual(result.target_ranks, [null]);
    assert.equal(result.recall_at_10, 0);
    assert.equal(result.rr_at_10, 0);
    assert.deepEqual(result.unknown_ranks, []);
  }
});

test("a required span must fit in one Entity; combining partial Entity ranges cannot complete it", () => {
  const evidence = span(7);
  const left = item({ ...evidence, end_byte: evidence.start_byte + 40 });
  const right = item({ ...evidence, start_byte: evidence.start_byte + 40 });
  const result = score([left, right], [target([evidence])]).entity;
  assert.equal(result.recall_at_10, 0);
  assert.deepEqual(result.unknown_ranks, []);
});

test("completion ranks preserve duplicate positions and cutoff boundaries", () => {
  const gold = target([span(7), span(9)]);
  const items = [
    item(span(7)),
    item(span(7)),
    filler(),
    filler(),
    filler(),
    item(span(9)),
  ];
  const result = score(items, [gold]).entity;
  assert.deepEqual(result.target_ranks, [6]);
  assert.equal(result.recall_at_1, 0);
  assert.equal(result.recall_at_5, 0);
  assert.equal(result.recall_at_10, 1);
  assert.equal(result.rr_at_10, 1 / 6);
  assert.equal(result.ndcg_at_10, 1 / Math.log2(7));
  const outside = [
    item(span(7)),
    ...Array.from({ length: 9 }, filler),
    item(span(9)),
  ];
  assert.equal(score(outside, [gold]).entity.recall_at_10, 0);
});

test("multiple completed alternatives use the earliest completion and never add extra target credit", () => {
  const gold = target([span(2)], [span(7), span(9)]);
  const result = score(
    [item(span(7)), item(span(9)), item(span(2)), item(span(2))],
    [gold],
  ).entity;
  assert.deepEqual(result.target_ranks, [2]);
  assert.equal(result.n_relevant, 1);
  assert.equal(result.recall_at_10, 1);
  assert.equal(result.ndcg_at_10, 1 / Math.log2(3));
});

test("uncertain evidence uses the completion rank of all members, not the first uncertain member", () => {
  const gold = target([span(7), span(9)]);
  const unknown = item(span(7), { source_bytes: undefined });
  const possible = score([unknown, item(span(9))], [gold]).entity;
  assert.equal(possible.hit_at_1, 0);
  assert.equal(possible.recall_at_1, 0);
  assert.equal(possible.recall_at_5, null);
  assert.equal(possible.rr_at_10, null);
  assert.deepEqual(possible.unknown_ranks, [
    { rank: 2, target_ids: [gold.id] },
  ]);

  const knownLater = score(
    [unknown, item(span(9)), item(span(7))],
    [gold],
  ).entity;
  assert.deepEqual(knownLater.target_ranks, [3]);
  assert.equal(knownLater.recall_at_5, 1);
  assert.equal(knownLater.rr_at_10, null);
  assert.equal(knownLater.ndcg_at_10, null);
});

test("a known alternative remains definitive when uncertainty cannot finish another set earlier", () => {
  const gold = target([span(2)], [span(7), span(9)]);
  const unknown = item(span(7), { source_bytes: undefined });
  const earlierKnown = score(
    [item(span(2)), unknown, item(span(9))],
    [gold],
  ).entity;
  assert.equal(earlierKnown.rr_at_10, 1);
  assert.equal(earlierKnown.ndcg_at_10, 1);
  assert.deepEqual(earlierKnown.unknown_ranks, []);
  const incompleteOther = score([unknown, item(span(2))], [gold]).entity;
  assert.equal(incompleteOther.recall_at_1, 0);
  assert.equal(incompleteOther.rr_at_10, 0.5);
  assert.deepEqual(incompleteOther.unknown_ranks, []);
});

test("grouped and flat targets share fixed per-file ideal gains while File scores stay unchanged", () => {
  const grouped = target([span(7), span(9)]);
  const ordinary = { id: "ordinary", path, ...span(12) };
  const items = [item(span(7)), item(span(12)), item(span(9))];
  const result = score(items, [grouped, ordinary]);
  assert.deepEqual(result.entity.target_ranks, [3, 2]);
  assert.equal(result.entity.n_relevant, 2);
  assert.equal(result.entity.recall_at_5, 1);
  assert.equal(
    result.entity.ndcg_at_10,
    (1 / Math.log2(4) + 1 / Math.log2(3)) / 2,
  );
  const flat = { id: grouped.id, path, ...span(7) };
  assert.deepEqual(result.file, score(items, [flat, ordinary]).file);
});

test("grouped uncertainty propagates through repeated scores without dropping the question's File vote", () => {
  const gold = target([span(7), span(9)]);
  const calls = Array.from({ length: 5 }, (_, i) => ({
    repetition: i + 1,
    status: "scored",
    execution_status: "success",
    items: ranked([item(span(7)), item(span(9))]),
  }));
  calls[0].items[0].source_bytes = undefined;
  const mean = summarizeRepeatedQaRetrieval(calls, pair([gold]), {
    entityByteRanges: true,
  });
  assert.equal(mean.entity.recall_at_1, 0);
  assert.equal(mean.entity.recall_at_5, null);
  assert.equal(mean.entity.scored_calls.recall_at_5, 4);
  assert.equal(mean.file.recall_at_1, 1);
});

test("empty or malformed evidence alternatives are rejected rather than scored as misses", () => {
  for (const sets of [
    null,
    [],
    [{ id: "empty", spans: [] }],
    [{ id: "bad", spans: [{ ...span(7), end_byte: 0 }] }],
    [{ id: "bad", spans: [{ ...span(7), start_line: 0 }] }],
    [{ id: "bad", spans: [{ ...span(7), path: "docs/other.md" }] }],
    [
      { id: "same", spans: [span(7)] },
      { id: "same", spans: [span(9)] },
    ],
  ]) {
    assert.throws(
      () => score([], [{ id: "article", path, evidence_sets: sets }]),
      /evidence/,
    );
  }
});
