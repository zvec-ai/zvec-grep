import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";
import {
  compareCombinedResults,
  compareRuns,
  markdownVersionComparison,
} from "../expansion/compare.mjs";
import { QUALITY_METRICS } from "../reports/ci.mjs";
import { QA_ENTITY_METRICS } from "../reports/zg.mjs";

const baseSha = "a".repeat(40),
  headSha = "b".repeat(40),
  harnessSha = "c".repeat(40);
const metadata = {
  baseline: { repository: "upstream/zg", commit: baseSha },
  candidate: { repository: "fork/zg", commit: headSha },
};
function result(commit, value = 0.5) {
  const rows = ["hybrid", "fts", "vector"].map((mode) => ({
    mode,
    label: `zg-${mode}`,
    metrics: Object.fromEntries(
      QUALITY_METRICS.map((metric) => [metric, value]),
    ),
  }));
  return {
    status: "success",
    candidate: { commit },
    harness_commit: harnessSha,
    sweqa: { status: "success", model: "local/code", rows },
    pilots: Object.fromEntries(
      ["beir", "duretrieval", "quarry"].map((name) => [
        name,
        {
          status: "success",
          model: "local/multilingual",
          report: { summary: structuredClone(rows) },
        },
      ]),
    ),
  };
}

test("all suites and modes report signed head-minus-base deltas with pinned commit links", () => {
  const before = result(baseSha, 0.75),
    after = result(headSha, 0.5);
  after.pilots.beir.report.summary[0].metrics.file_hit_at_1 = 1;
  const comparison = compareCombinedResults(before, after, metadata);
  assert.equal(comparison.rows.length, 12);
  assert.equal(comparison.rows[0].metrics.file_hit_at_1.delta, -0.25);
  assert.equal(comparison.rows[3].metrics.file_hit_at_1.delta, 0.25);
  const markdown = markdownVersionComparison(comparison);
  assert.match(markdown, /0\.7500 \/ 0\.5000 \/ -0\.2500/);
  assert.match(markdown, /0\.7500 \/ 1\.0000 \/ \+0\.2500/);
  assert.ok(
    markdown.includes(`https://github.com/upstream/zg/commit/${baseSha}`),
  );
  assert.ok(markdown.includes(`https://github.com/fork/zg/commit/${headSha}`));
  const identical = compareCombinedResults(
    before,
    result(headSha, 0.75),
    metadata,
  );
  assert.ok(
    identical.rows.every((row) =>
      QUALITY_METRICS.every((metric) => row.metrics[metric].delta === 0),
    ),
  );
});

test("incomplete, mixed-harness, wrong-commit and mixed-model results are rejected", () => {
  for (const mutate of [
    (before) => {
      before.status = "failed";
    },
    (before) => {
      before.candidate.commit = headSha;
    },
    (before) => {
      before.harness_commit = headSha;
    },
    (before) => {
      before.pilots.beir.status = "failed";
    },
    (before) => {
      before.pilots.quarry.model = "remote/model";
    },
    (before) => {
      before.sweqa.rows.pop();
    },
    (before) => {
      before.sweqa.rows[0].metrics.ndcg_at_10 = null;
    },
  ]) {
    const before = result(baseSha);
    mutate(before);
    assert.throws(() =>
      compareCombinedResults(before, result(headSha), metadata),
    );
  }
});

test("Entity diagnostics retain each version's scored question counts", () => {
  const before = result(baseSha);
  const after = result(headSha);
  for (const [run, recall, count] of [
    [before, 0.125, 13],
    [after, 0.25, 14],
  ]) {
    const entity = Object.fromEntries(
      QA_ENTITY_METRICS.map(([, metric]) => [metric, 0]),
    );
    entity.metric_task_counts = Object.fromEntries(
      QA_ENTITY_METRICS.map(([, metric]) => [metric, 17]),
    );
    entity.recall_at_5 = null;
    entity.metric_task_counts.recall_at_5 = 0;
    entity.recall_at_10 = recall;
    entity.metric_task_counts.recall_at_10 = count;
    run.sweqa.rows[0].qa_retrieval = {
      diagnostic: { eligible_tasks: 17, entity },
    };
  }
  const comparison = compareCombinedResults(before, after, metadata);
  assert.equal(
    comparison.rows.filter((row) => row.entity_diagnostic).length,
    1,
  );
  const markdown = markdownVersionComparison(comparison);
  assert.match(markdown, /SWE-QA20 Entity retrieval · Diagnostic/);
  assert.match(markdown, /Recall@1 \| Recall@5 \| Recall@10/);
  assert.match(
    markdown,
    /\| hybrid \| Baseline \|.*0\.0000 \(n=17\/17\) \| N\/A \(n=0\/17\) \| 0\.1250 \(n=13\/17\)/,
  );
  assert.match(
    markdown,
    /\| hybrid \| Candidate \|.*0\.0000 \(n=17\/17\) \| N\/A \(n=0\/17\) \| 0\.2500 \(n=14\/17\)/,
  );
});

test("raw-report comparison rejects missing evidence and substituted SWE-QA versions", async () => {
  const baseline = { summary: result(baseSha), reports: {} };
  await assert.rejects(
    compareRuns(baseline, baseline, metadata),
    /missing repositories/,
  );
  baseline.reports.zg = { repositories: [{ candidate_commit: headSha }] };
  await assert.rejects(
    compareRuns(baseline, baseline, metadata),
    /wrong SWE-QA candidate/,
  );
});

test("missing artifacts still produce an explicitly incomplete comment report without fabricated scores", async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "zg-version-comparison-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const process = spawnSync(
    globalThis.process.execPath,
    [
      fileURLToPath(new URL("../expansion/compare.mjs", import.meta.url)),
      "--baseline",
      join(directory, "missing-base"),
      "--candidate",
      join(directory, "missing-head"),
      "--output",
      directory,
    ],
    {
      env: {
        ...globalThis.process.env,
        RETRIEVAL_BASELINE_REPOSITORY: metadata.baseline.repository,
        RETRIEVAL_BASELINE_COMMIT: baseSha,
        RETRIEVAL_CANDIDATE_REPOSITORY: metadata.candidate.repository,
        RETRIEVAL_CANDIDATE_COMMIT: headSha,
      },
      encoding: "utf8",
    },
  );
  assert.equal(process.status, 1, process.stderr);
  const report = JSON.parse(
    await readFile(join(directory, "summary.json"), "utf8"),
  );
  assert.equal(report.status, "failed");
  assert.deepEqual(report.rows, []);
  assert.match(
    await readFile(join(directory, "summary.md"), "utf8"),
    /Missing or invalid results are not zero scores/,
  );
});
