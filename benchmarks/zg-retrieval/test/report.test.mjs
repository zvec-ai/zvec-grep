import assert from "node:assert/strict";
import test from "node:test";
import { markdownReport } from "../reports/zg.mjs";

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
    preview: "mcp-default",
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
  assert.match(
    text,
    /mean of five calls per question and mode; Rust MCP default/,
  );
  assert.match(text, /weights those repositories equally/);
  assert.match(text, /does not send a preview override/);
  assert.match(text, /<details>/);
  assert.match(text, /100\*\* successful valid calls/);
  assert.doesNotMatch(
    text,
    /Legacy|anchor|nDCG@5|By category|Preparation and latency|Mean output bytes|short|Paired retrieval/,
  );
  assert.equal((text.match(/\| Arm \|/g) ?? []).length, 1);
});
