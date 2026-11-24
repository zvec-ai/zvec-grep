import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";
import {
  compareCombinedResults,
  compareRuns,
  markdownVersionComparison,
  readComparisonInput,
} from "../expansion/compare.mjs";
import { QUALITY_METRICS } from "../reports/ci.mjs";
import { QA_ENTITY_METRICS } from "../reports/zg.mjs";
import { QA_SCORING } from "../core/suite.mjs";
import { summarizeIndexSettings } from "../core/index-settings.mjs";
import {
  PILOT_FILE_RECALL_METRICS,
  PILOT_NAMES,
  PILOT_QA_SCORING,
  PILOT_SUITES,
} from "../expansion/config.mjs";

const baseSha = "a".repeat(40),
  headSha = "b".repeat(40),
  harnessSha = "c".repeat(40);
const metadata = {
  baseline: { repository: "upstream/zg", commit: baseSha },
  candidate: { repository: "fork/zg", commit: headSha },
};
function tableRow(markdown, suite, mode, level) {
  const section = markdown.split(`### ${suite}\n`)[1]?.split("\n### ")[0];
  const row = section
    ?.split("\n")
    .find((line) => line.startsWith(`| ${mode} | ${level} |`));
  assert.ok(row, `${suite}/${mode}/${level}: missing row`);
  return row
    .split("|")
    .slice(1, -1)
    .map((cell) => cell.trim());
}
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
    sweqa: {
      status: "success",
      preview: "full",
      model: "local/code",
      qa_scoring: { ...QA_SCORING },
      rows,
    },
    pilots: Object.fromEntries(
      PILOT_NAMES.map((name) => [
        name,
        {
          status: "success",
          model: "local/multilingual",
          report: { preview: "full", summary: structuredClone(rows) },
        },
      ]),
    ),
  };
}

test("all suites and modes retain exact deltas and render compact baseline-to-candidate percentages", () => {
  const before = result(baseSha, 0.75),
    after = result(headSha, 0.5);
  after.pilots.beir.report.summary[0].metrics.file_hit_at_1 = 1;
  const comparison = compareCombinedResults(before, after, metadata);
  assert.equal(comparison.rows.length, 12);
  assert.equal(comparison.preview, "full");
  assert.deepEqual(comparison.qa_scoring, QA_SCORING);
  assert.equal(comparison.rows[0].metrics.file_hit_at_1.delta, -0.25);
  assert.equal(comparison.rows[3].metrics.file_hit_at_1.delta, 0.25);
  const markdown = markdownVersionComparison(comparison);
  assert.doesNotMatch(markdown, /Stage capture incomplete/);
  assert.deepEqual(
    [...markdown.matchAll(/^### (.+)$/gm)].map((match) => match[1]),
    ["SWE-QA20", "BEIR", "DuRetrieval", "Quarry"],
  );
  assert.equal((markdown.match(/^\| Mode \| Level \|/gm) ?? []).length, 4);
  assert.equal(
    (
      markdown.match(
        /Indexed corpus: \*\*N\/A Entities · N\/A Fragments\*\*\./g,
      ) ?? []
    ).length,
    4,
  );
  for (const suite of ["SWE-QA20", "BEIR", "DuRetrieval", "Quarry"])
    for (const mode of ["fts", "vector", "hybrid"]) {
      const file = tableRow(markdown, suite, mode, "File");
      assert.equal(file.length, 10);
      assert.deepEqual(file.slice(5, 8), ["N/A", "N/A", "N/A"]);
      assert.deepEqual(
        tableRow(markdown, suite, mode, "Entity").slice(2),
        Array(8).fill("N/A"),
      );
    }
  assert.match(markdown, /Both versions explicitly request `preview: "full"`/);
  assert.match(markdown, /75\.00% → 50\.00%/);
  assert.match(markdown, /75\.00% → 100\.00%/);
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

test("indexed corpus counts compare once per suite and preserve unknown totals", () => {
  const before = result(baseSha),
    after = result(headSha);
  const counts = {
    entities: 1007,
    fragments: 2011,
    corpora: [
      { id: "first", entities: 1000, fragments: 2000 },
      { id: "second", entities: 7, fragments: 11 },
    ],
  };
  before.sweqa.index_counts = structuredClone(counts);
  after.sweqa.index_counts = structuredClone(counts);
  before.pilots.beir.report.index_counts = structuredClone(counts);
  after.pilots.beir.report.index_counts = {
    entities: 1008,
    fragments: 2011,
    corpora: [
      { id: "first", entities: 1001, fragments: 2000 },
      { id: "second", entities: 7, fragments: 11 },
    ],
  };
  after.pilots.duretrieval.report.index_counts = {
    entities: null,
    fragments: 11,
    corpora: [
      { id: "first", entities: 7, fragments: 10 },
      { id: "second", entities: null, fragments: 1 },
    ],
  };
  const comparison = compareCombinedResults(before, after, metadata);
  assert.deepEqual(comparison.index_counts.sweqa, {
    baseline: counts,
    candidate: counts,
  });
  assert.deepEqual(comparison.index_counts.quarry, {
    baseline: null,
    candidate: null,
  });
  assert.equal(comparison.index_counts.duretrieval.candidate.entities, null);
  const markdown = markdownVersionComparison(comparison);
  assert.deepEqual(
    markdown.split("\n").filter((line) => line.startsWith("Indexed corpus:")),
    [
      "Indexed corpus: **1,007 Entities · 2,011 Fragments**.",
      "Indexed corpus: **1,007 → 1,008 Entities · 2,011 Fragments**.",
      "Indexed corpus: **N/A Entities · N/A → 11 Fragments**.",
      "Indexed corpus: **N/A Entities · N/A Fragments**.",
    ],
  );
  before.sweqa.index_counts.corpora[0].entities = 0;
  assert.deepEqual(comparison.index_counts.sweqa.baseline, counts);
  before.sweqa.index_counts = { ...counts, fragments: 2012 };
  assert.throws(() => compareCombinedResults(before, after, metadata));
});

test("comparison links HTML without forwarding stage summaries to comments", () => {
  const before = result(baseSha);
  const after = result(headSha);
  const quality = compareCombinedResults(before, after, metadata);
  before.sweqa.gold_stages = { version: 1, groups: [] };
  after.sweqa.gold_stages = { error: "unavailable stage evidence" };
  for (const pilot of Object.values(after.pilots))
    pilot.report.gold_stages = after.sweqa.gold_stages;
  const comparison = compareCombinedResults(before, after, metadata);
  assert.deepEqual(comparison, quality);
  const traceUrl = "https://github.com/upstream/zg/actions/runs/1/artifacts/2";
  const markdown = markdownVersionComparison(comparison, { traceUrl });
  assert.ok(markdown.includes(`[Download stage trace (HTML)](${traceUrl})`));
  assert.equal((markdown.match(/Download stage trace/g) ?? []).length, 1);
  assert.doesNotMatch(
    markdown,
    /Gold coverage by stage|Gold covered by index|Before rule ranking|After rule ranking|Stage capture/,
  );
  assert.doesNotMatch(
    markdownVersionComparison(comparison),
    /Download stage trace/,
  );
  assert.equal((markdown.match(/^\| Mode \| Level \|/gm) ?? []).length, 4);
});

test("a newly registered suite retains its trusted report title", async () => {
  const name = "new-pilot";
  PILOT_SUITES[name] = { report: { comparison: "New pilot" } };
  PILOT_NAMES.push(name);
  try {
    // Load the comparator as it would start after the registry was extended.
    const extended = await import("../expansion/compare.mjs?new-suite-test");
    const before = result(baseSha);
    const after = result(headSha);
    const comparison = extended.compareCombinedResults(before, after, metadata);
    const markdown = extended.markdownVersionComparison(comparison);
    assert.match(markdown, /^### New pilot$/m);
    assert.doesNotMatch(markdown, /Gold coverage by stage/);
  } finally {
    PILOT_NAMES.pop();
    delete PILOT_SUITES[name];
  }
});

test("model budgets propagate to each dataset without changing retrieval metrics", () => {
  const before = result(baseSha),
    after = result(headSha);
  const original = compareCombinedResults(before, after, metadata);
  const budget = {
    max_input_tokens: 1024,
    unit: "utf16_code_units",
    rounding: "floor",
    overlap_percent: 15,
    normal: { chars_per_100_tokens: 185, max_length: 1894, overlap: 284 },
    dense: { chars_per_100_tokens: 100, max_length: 1024, overlap: 153 },
  };
  for (const suite of [after.sweqa, ...Object.values(after.pilots)]) {
    (suite.report ?? suite).index_settings = summarizeIndexSettings([
      { id: "corpus", model: suite.model, input_budget: budget },
    ]);
  }
  const comparison = compareCombinedResults(before, after, metadata);
  assert.deepEqual(comparison.rows, original.rows);
  assert.equal(comparison.index_settings.sweqa.baseline, null);
  assert.deepEqual(
    comparison.index_settings.beir.candidate.input_budget,
    budget,
  );
  const markdown = markdownVersionComparison(comparison);
  assert.equal((markdown.match(/^Embedding:/gm) ?? []).length, 4);
  assert.match(markdown, /N\/A → 1,024 tokens/);
  assert.match(markdown, /before metadata allowance/);
  after.sweqa.index_settings = summarizeIndexSettings([
    { id: "corpus", model: "local/wrong", input_budget: budget },
  ]);
  assert.throws(() => compareCombinedResults(before, after, metadata), /model/);
});

test("historical unknown index models remain comparable without hiding a wrong known model", () => {
  const before = result(baseSha),
    after = result(headSha);
  const unknown = [
    { id: "a", model: null, input_budget: null },
    { id: "b", model: null, input_budget: null },
  ];
  for (const suite of [before.sweqa, ...Object.values(before.pilots)])
    (suite.report ?? suite).index_settings = summarizeIndexSettings(unknown);
  const comparison = compareCombinedResults(before, after, metadata);
  assert.equal(comparison.index_settings.sweqa.baseline.model, null);
  assert.equal(comparison.index_settings.beir.baseline.input_budget, null);
  assert.match(
    markdownVersionComparison(comparison),
    /ZG input limit: \*\*N\/A\*\*/,
  );
  for (const suite of [before.sweqa, ...Object.values(before.pilots)]) {
    const target = suite.report ?? suite;
    const original = target.index_settings;
    target.index_settings = summarizeIndexSettings([
      unknown[0],
      { ...unknown[1], model: suite.model },
    ]);
    assert.doesNotThrow(() => compareCombinedResults(before, after, metadata));
    target.index_settings = summarizeIndexSettings([
      unknown[0],
      { ...unknown[1], model: "local/wrong" },
    ]);
    assert.equal(target.index_settings.model, null);
    assert.throws(
      () => compareCombinedResults(before, after, metadata),
      /model/,
    );
    target.index_settings = original;
  }
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
      before.sweqa.preview = "mcp-default";
    },
    (before) => {
      before.pilots.quarry.report.preview = "short";
    },
    (before) => {
      before.sweqa.rows[0].metrics.ndcg_at_10 = null;
    },
    (before) => {
      delete before.sweqa.qa_scoring;
    },
    (before) => {
      before.sweqa.qa_scoring = { version: 1, entity_match: "contains" };
    },
  ]) {
    const before = result(baseSha);
    mutate(before);
    assert.throws(() =>
      compareCombinedResults(before, result(headSha), metadata),
    );
  }
});

test("File and Entity diagnostics retain their own scores and question counts", () => {
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
    const file = {
      recall_at_1: 0.25,
      recall_at_5: 0.5,
      recall_at_10: recall + 0.5,
      metric_task_counts: {
        recall_at_1: 17,
        recall_at_5: 17,
        recall_at_10: 17,
      },
    };
    run.sweqa.rows[0].qa_retrieval = {
      diagnostic: { eligible_tasks: 17, file, entity },
    };
  }
  const comparison = compareCombinedResults(before, after, metadata);
  assert.equal(
    comparison.rows.filter((row) => row.entity_diagnostic).length,
    1,
  );
  const diagnostic = comparison.rows[0].entity_diagnostic;
  assert.equal(diagnostic.baseline.eligible_tasks, 17);
  assert.equal(diagnostic.candidate.eligible_tasks, 17);
  assert.equal(diagnostic.baseline.entity.metric_task_counts.recall_at_10, 13);
  assert.equal(diagnostic.candidate.entity.metric_task_counts.recall_at_10, 14);
  const markdown = markdownVersionComparison(comparison);
  assert.match(markdown, /Diagnostic QA results/);
  assert.doesNotMatch(markdown, /(?:%|N\/A) \([^)]*\/[^)]*\)/);
  assert.match(
    markdown,
    /Hit@1 \| Hit@5 \| Hit@10 \| Recall@1 \| Recall@5 \| Recall@10 \| MRR@10 \| nDCG@10/,
  );
  assert.deepEqual(
    tableRow(markdown, "SWE-QA20", "hybrid", "File").slice(5, 8),
    ["25.00%", "50.00%", "62.50% → 75.00%"],
  );
  assert.deepEqual(
    tableRow(markdown, "SWE-QA20", "hybrid", "Entity").slice(5, 8),
    ["0.00%", "N/A", "12.50% → 25.00%"],
  );
  // A partially available QA table must never fall back to original File Gold.
  assert.match(tableRow(markdown, "SWE-QA20", "hybrid", "File")[2], /^N\/A/);
  assert.deepEqual(
    tableRow(markdown, "SWE-QA20", "fts", "File").slice(2),
    Array(8).fill("N/A"),
  );
  assert.doesNotMatch(markdown, /\| hybrid \| (Baseline|Candidate) \|/);
  assert.doesNotMatch(markdown, /<details>|File ranking — all four suites/);
});

test("compact cells hide counts and retain rounded differences and missing versions", () => {
  const before = result(baseSha),
    after = result(headSha);
  const make = (value, count, eligible = 17) => ({
    eligible_tasks: eligible,
    entity: {
      recall_at_1: value,
      recall_at_5: null,
      recall_at_10: 0,
      metric_task_counts: {
        recall_at_1: count,
        recall_at_5: 0,
        recall_at_10: 17,
      },
    },
  });
  for (const [baseline, candidate, expected] of [
    [make(0.5, 13), make(0.5, 13), "50.00%"],
    [make(0.5, 13), make(0.5, 14), "50.00%"],
    [make(0.5, 13), make(0.5, 13, 18), "50.00%"],
    [make(0.5, 13), make(0.500001, 13), "50.00% → 50.00%"],
    [null, make(0.5, 13), "N/A → 50.00%"],
  ]) {
    const comparison = compareCombinedResults(before, after, metadata);
    comparison.rows[0].entity_diagnostic = { baseline, candidate };
    const saved = structuredClone(comparison);
    const markdown = markdownVersionComparison(comparison);
    assert.equal(
      tableRow(markdown, "SWE-QA20", "hybrid", "Entity")[5],
      expected,
    );
    assert.deepEqual(comparison, saved);
    if (baseline && candidate)
      assert.deepEqual(
        tableRow(markdown, "SWE-QA20", "hybrid", "Entity").slice(6, 8),
        ["N/A", "0.00%"],
      );
  }
  const same = markdownVersionComparison(
    compareCombinedResults(before, after, metadata),
  );
  assert.deepEqual(
    tableRow(same, "SWE-QA20", "hybrid", "File").slice(2, 5),
    Array(3).fill("50.00%"),
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
  assert.match(
    await readFile(join(directory, "retrieval-trace.html"), "utf8"),
    /Comparison incomplete/,
  );
});

test("combined artifacts retain per-call trace sidecars and tolerate older artifacts", async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "zg-trace-artifacts-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const args = [];
  const evidence = {
    summary: { version: 2 },
    calls: [{ task_id: "original", repetition: 5 }],
  };
  for (const name of ["zg", ...PILOT_NAMES]) {
    const source = join(directory, name);
    await mkdir(source);
    await writeFile(join(source, "report.json"), "null\n");
    if (name === "zg")
      await writeFile(
        join(source, "gold-stages.json"),
        JSON.stringify(evidence),
      );
    args.push(`--${name}`, join(source, "report.json"));
  }
  const output = join(directory, "combined");
  const child = spawnSync(
    globalThis.process.execPath,
    [
      fileURLToPath(new URL("../expansion/combined.mjs", import.meta.url)),
      ...args,
      "--output",
      output,
    ],
    { encoding: "utf8" },
  );
  assert.equal(child.status, 1, child.stderr); // Intentionally missing quality reports.
  const read = await readComparisonInput(output);
  assert.deepEqual(read.traces.zg, evidence);
  assert.equal(read.traces.beir, null);
  assert.equal(read.summary.status, "failed");
  await writeFile(join(output, "reports", "beir-gold-stages.json"), "{");
  const malformed = await readComparisonInput(output);
  assert.ok(malformed.traces.beir.error);
  assert.deepEqual(malformed.summary, read.summary);
  await rm(join(directory, "zg", "gold-stages.json"));
  const repeated = spawnSync(
    globalThis.process.execPath,
    [
      fileURLToPath(new URL("../expansion/combined.mjs", import.meta.url)),
      ...args,
      "--output",
      output,
    ],
    { encoding: "utf8" },
  );
  assert.equal(repeated.status, 1, repeated.stderr);
  assert.equal((await readComparisonInput(output)).traces.zg, null);
});

test("rejected artifact identity cannot attribute stage evidence to the requested commits", async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "zg-trace-identity-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const input = join(directory, "input");
  await mkdir(join(input, "reports"), { recursive: true });
  const summary = result(headSha);
  summary.sweqa.gold_stages = {
    version: 2,
    calls: 5,
    observed_calls: 5,
    invalid_calls: 0,
    groups: [],
  };
  await writeFile(join(input, "summary.json"), JSON.stringify(summary));
  for (const name of ["zg", ...PILOT_NAMES])
    await writeFile(join(input, "reports", `${name}.json`), "null\n");
  const child = spawnSync(
    globalThis.process.execPath,
    [
      fileURLToPath(new URL("../expansion/compare.mjs", import.meta.url)),
      "--baseline",
      input,
      "--candidate",
      input,
      "--output",
      directory,
    ],
    {
      encoding: "utf8",
      env: {
        ...globalThis.process.env,
        RETRIEVAL_BASELINE_REPOSITORY: metadata.baseline.repository,
        RETRIEVAL_BASELINE_COMMIT: baseSha,
        RETRIEVAL_CANDIDATE_REPOSITORY: metadata.candidate.repository,
        RETRIEVAL_CANDIDATE_COMMIT: headSha,
      },
    },
  );
  assert.equal(child.status, 1, child.stderr);
  const comparison = JSON.parse(
    await readFile(join(directory, "summary.json"), "utf8"),
  );
  assert.match(comparison.errors[0], /wrong artifact commit/);
  const html = await readFile(join(directory, "retrieval-trace.html"), "utf8");
  const data = JSON.parse(
    html.match(
      /<script type="application\/json" id="trace-data">([\s\S]*?)<\/script>/,
    )[1],
  );
  for (const suite of data.suites)
    for (const side of ["baseline", "candidate"])
      assert.equal(suite[side].summary, null);
});

test("paired pilot reports render added scores only under the same Gold and scoring rules", () => {
  const before = result(baseSha);
  const after = result(headSha);
  for (const side of [before, after]) {
    const report = side.pilots.beir.report;
    report.qa_gold = { id: "beir20-entity-v2", sha256: "a".repeat(64) };
    report.qa_scoring = { ...PILOT_QA_SCORING };
    for (const row of report.summary) {
      for (const key of PILOT_FILE_RECALL_METRICS) row.metrics[key] = 0.375;
      row.qa_retrieval = {
        diagnostic: {
          eligible_tasks: 1,
          entity: {
            ...Object.fromEntries(
              QA_ENTITY_METRICS.map(([, key]) => [key, 0.25]),
            ),
            metric_task_counts: Object.fromEntries(
              QA_ENTITY_METRICS.map(([, key]) => [key, 1]),
            ),
          },
        },
      };
    }
  }
  const comparison = compareCombinedResults(before, after, metadata);
  const markdown = markdownVersionComparison(comparison);
  assert.deepEqual(
    tableRow(markdown, "BEIR", "fts", "File").slice(5, 8),
    Array(3).fill("37.50%"),
  );
  assert.deepEqual(
    tableRow(markdown, "BEIR", "fts", "Entity").slice(2),
    Array(8).fill("25.00%"),
  );
  assert.match(markdown, /Diagnostic Entity results/);
  after.pilots.beir.report.qa_scoring = {
    version: 1,
    entity_match: "contains",
    range: "source_bytes",
  };
  assert.throws(
    () => compareCombinedResults(before, after, metadata),
    /different pilot scoring rules/,
  );
  after.pilots.beir.report.qa_scoring = { ...PILOT_QA_SCORING };
  after.pilots.beir.report.qa_gold.sha256 = "b".repeat(64);
  assert.throws(
    () => compareCombinedResults(before, after, metadata),
    /different pilot Gold/,
  );
});
