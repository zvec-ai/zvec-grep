import assert from "node:assert/strict";
import test from "node:test";
import { markdownReport, qaRetrievalMarkdown } from "../reports/zg.mjs";

test("report has one three-arm table with Rust MCP output, fixed averages and per-query evidence", () => {
  const mode = {
    file_retrieval: {
      planned_tasks: 20,
      scored_tasks: 20,
      hit_at_1_count: 7,
      hit_at_5_count: 12,
      hit_at_10_count: 15,
      hit_at_1: 7 / 20,
      hit_at_5: 12 / 20,
      hit_at_10: 15 / 20,
      mrr_at_10: 0.4055555556,
    },
    ndcg: {
      query_count: 20,
      repository_count: 11,
      language_count: 1,
      query_mean: { ndcg_at_10: 0.5 },
      repository_macro: { ndcg_at_10: 0.292475 },
      language_macro: { ndcg_at_10: 0.292475 },
    },
    measurements: {
      output_bytes_mean: 1536,
      output_sample_count: 20,
      latency_ms_mean: 15,
      latency_ms_p50: 12.34567,
      latency_sample_count: 100,
    },
    ranking_repeatable_tasks: 18,
  };
  const report = {
    scope: "full-20-original-queries",
    observed_calls: 300,
    integrity_passed: true,
    expected_task_ids: Array.from({ length: 20 }, (_, i) => `task:${i}`),
    schema_version: 7,
    preview: "full",
    modes: { hybrid: mode, fts: mode, vector: mode },
    tasks: [],
    repositories: Array.from({ length: 11 }, (_, i) => ({
      repository: `owner/repo${i}`,
    })),
    integrity_errors: [],
  };
  const text = markdownReport(report);
  for (const mode of ["hybrid", "fts", "vector"])
    assert.ok(
      text.includes(
        `| zg-${mode} | 20/20 questions; 11/11 repositories | 18/20 | 0.3500 | 0.6000 | 0.7500 | 0.4056 | 0.2925 | 1.5000 | 15.0000 | 12.3457 |`,
      ),
    );
  assert.match(text, /20 original questions \/ 11 repositories/);
  assert.match(text, /mean of five calls per question and mode; Rust MCP full/);
  assert.match(text, /weights those repositories equally/);
  assert.match(text, /explicitly sets preview to full/);
  assert.match(text, /<details>/);
  assert.match(text, /100\*\* successful valid calls/);
  assert.doesNotMatch(
    text,
    /Legacy|anchor|nDCG@5|By category|Preparation and latency|Mean output bytes|short|Paired retrieval/,
  );
  assert.equal((text.match(/\| Arm \|/g) ?? []).length, 1);
});

function qaLevel(count = 2) {
  const names = [
    "hit_at_1",
    "hit_at_5",
    "hit_at_10",
    "recall_at_1",
    "recall_at_5",
    "recall_at_10",
    "mrr_at_10",
    "ndcg_at_10",
  ];
  return {
    ...Object.fromEntries(names.map((name) => [name, 0.5])),
    scored_tasks: count,
    metric_task_counts: Object.fromEntries(names.map((name) => [name, count])),
  };
}

test("QA tables show diagnostic denominators and withhold unapproved formal scores", () => {
  const qa = {
    planned_tasks: 4,
    formal: {
      eligible_tasks: 0,
      file: qaLevel(),
      entity: qaLevel(),
    },
    diagnostic: {
      eligible_tasks: 3,
      file: qaLevel(),
      entity: qaLevel(),
    },
    unavailable_tasks: [{ task_id: "ambiguous", reason: "ambiguous query" }],
  };
  qa.diagnostic.entity.recall_at_10 = null;
  qa.diagnostic.entity.metric_task_counts.recall_at_10 = 0;
  qa.diagnostic.entity.hit_at_1 = 0;
  const report = {
    modes: { hybrid: { qa_retrieval: qa } },
    expected_task_ids: ["a", "b", "c", "ambiguous"],
    tasks: [],
    repositories: [],
    integrity_passed: true,
    integrity_errors: [],
  };
  const text = markdownReport(report);
  assert.match(text, /### QA File Recall/);
  assert.match(text, /### QA Entity retrieval/);
  assert.match(text, /Recall@1 \| Recall@5 \| Recall@10/);
  assert.match(text, /\| Formal \| zg-hybrid \| 0\/4 \| N\/A \(n=0\/0\)/);
  const formal = text
    .split("\n")
    .filter((line) => line.startsWith("| Formal |"));
  assert.ok(formal.every((line) => !line.includes("0.5000")));
  assert.match(
    text,
    /\| Diagnostic \| zg-hybrid \| 3\/4 \| 0\.5000 \(n=2\/3\)/,
  );
  assert.match(text, /0\.0000 \(n=2\/3\)/);
  assert.match(text, /N\/A \(n=0\/3\)/);
  assert.match(text, /do not establish formal Entity performance/);
  assert.match(text, /Unknown results never become zeros/);
  assert.match(text, /QA scoring unavailable for 1\/4 questions/);
});

test("QA table renders approved metrics without turning an unscored metric into zero", () => {
  const formal = {
    eligible_tasks: 1,
    file: qaLevel(1),
    entity: qaLevel(1),
  };
  formal.entity.recall_at_10 = null;
  formal.entity.metric_task_counts.recall_at_10 = 0;
  const text = qaRetrievalMarkdown(
    [
      [
        "zg-hybrid",
        {
          planned_tasks: 1,
          formal,
          diagnostic: { eligible_tasks: 0, file: null, entity: null },
          unavailable_tasks: [],
        },
      ],
    ],
    1,
  ).join("\n");
  assert.match(text, /\| Formal \| zg-hybrid \| 1\/1 \| 0\.5000 \(n=1\/1\)/);
  assert.match(text, /N\/A \(n=0\/1\)/);
  assert.doesNotMatch(text, /0\.0000/);
});
