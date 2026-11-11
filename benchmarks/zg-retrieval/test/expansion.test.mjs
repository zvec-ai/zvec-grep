import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { loadPilot } from "../expansion/datasets.mjs";
import { locateFullEntity, summarizePilotEntity } from "../expansion/gold.mjs";
import { PILOT_PREVIEW, PILOT_PROTOCOL } from "../expansion/config.mjs";
import {
  searchArguments,
  summarizePilotBreakdown,
  summarizePilotRows,
  markdownPilotReport,
  invalidateGroupRows,
} from "../expansion/run.mjs";
import { buildCombined, markdownCombined } from "../expansion/combined.mjs";
import { parseVisibleResponse } from "../engines/zg/parse.mjs";
import { scoreFileRetrieval } from "../metrics/files.mjs";
import { scoreNdcg } from "../metrics/ndcg.mjs";
import { summarizeRepeatedQuality } from "../metrics/repetitions.mjs";

test("all pilot routes request full preview without changing the original query", () => {
  const task = { query: "original question" };
  for (const mode of ["hybrid", "fts", "vector"]) {
    assert.deepEqual(searchArguments(task, "/corpus", mode), {
      root: "/corpus",
      ...(mode === "hybrid" ? { query: task.query } : { [mode]: [task.query] }),
      limit: 10,
      preview: "full",
      autoUpdate: false,
      freshness: "eventual",
      preferSymbol: false,
    });
  }
});

test("expanded runner uses the flag-style Rust management interface", async () => {
  const source = await readFile(
    new URL("../expansion/run.mjs", import.meta.url),
    "utf8",
  );
  for (const command of ["install", "server", "index"])
    assert.match(source, new RegExp(`"--${command}"`));
  assert.doesNotMatch(source, /\[\s*"(?:install|server|index)"/);
});

test("expanded locks preserve the original ten questions and cover new datasets and languages", async () => {
  const beir = await loadPilot("beir");
  const duretrieval = await loadPilot("duretrieval");
  const quarry = await loadPilot("quarry");
  assert.equal(beir.lock.tasks.length, 20);
  assert.equal(duretrieval.lock.tasks.length, 10);
  assert.equal(quarry.lock.tasks.length, 20);
  assert.equal(beir.lock.model, "local/potion-multilingual-128m");
  assert.equal(duretrieval.lock.model, "local/potion-multilingual-128m");
  assert.equal(quarry.lock.model, "local/potion-code-16m-v2");
  assert.equal(beir.model, beir.lock.model);
  assert.equal(duretrieval.model, duretrieval.lock.model);
  assert.equal(quarry.model, quarry.lock.model);
  assert.equal(
    (await loadPilot("beir", { embedding: "remote" })).model,
    "qwen/qwen3.7-text-embedding",
  );
  assert.equal(duretrieval.lock.corpus_documents, 100001);
  assert.equal(duretrieval.lock.source.query_count, 2000);
  assert.equal(duretrieval.lock.source.qrel_count, 9839);
  assert.ok(duretrieval.lock.tasks.every((task) => task.qrels.length > 0));
  assert.equal(
    new Set(quarry.lock.tasks.map((task) => task.source_task_id)).size,
    20,
  );
  assert.equal(
    new Set(quarry.lock.tasks.map((task) => task.revision)).size,
    20,
  );
  assert.ok(
    quarry.lock.tasks.every((task) =>
      task.positive_units.every((unit) => unit.revision === task.revision),
    ),
  );
  assert.ok(
    beir.lock.tasks.every((task) =>
      task.qrels.every((row) => row.relevance > 0),
    ),
  );
  assert.deepEqual(
    Object.fromEntries(
      beir.lock.datasets.map((dataset) => [dataset.id, dataset.tasks.length]),
    ),
    { scifact: 10, nfcorpus: 4, arguana: 3, fiqa: 3 },
  );
  assert.deepEqual(
    Object.fromEntries(
      [...new Set(quarry.lock.tasks.map((task) => task.language))].map(
        (language) => [
          language,
          quarry.lock.tasks.filter((task) => task.language === language).length,
        ],
      ),
    ),
    {
      Go: 11,
      Python: 2,
      Rust: 2,
      JavaScript: 1,
      TypeScript: 1,
      Java: 1,
      "C#": 1,
      C: 1,
    },
  );
  const oldBeir = JSON.parse(
    await readFile(
      new URL("../expansion/data/beir-scifact10.json", import.meta.url),
    ),
  );
  const oldQuarry = JSON.parse(
    await readFile(new URL("../expansion/data/quarry10.json", import.meta.url)),
  );
  assert.deepEqual(beir.lock.datasets[0].tasks, oldBeir.tasks);
  for (let index = 0; index < 10; index++)
    for (const key of [
      "id",
      "source_task_id",
      "query",
      "revision",
      "positive_units",
    ])
      assert.deepEqual(
        quarry.lock.tasks[index][key],
        oldQuarry.tasks[index][key],
      );
  assert.ok(
    beir.lock.tasks
      .filter((task) => task.dataset === "arguana")
      .every((task) =>
        task.qrels.every((row) => row.document_id !== task.source_id),
      ),
  );
});

test("pilot report preserves completed scores and calls out missing tasks", () => {
  const success = {
    task_id: "a",
    mode: "hybrid",
    status: "success",
    targets: [{ path: "target.md" }],
    items: [{ rank: 1, path: "target.md", matched_by: "fts+vector" }],
    file: { hit_at_1: 1, hit_at_5: 1, hit_at_10: 1, rr_at_10: 1 },
    ndcg: { ndcg_at_10: 1 },
    output_bytes: 1024,
    calls: [100, 110, 120, 130, 140].map((latency_ms, index) => ({
      repetition: index + 1,
      status: "success",
      latency_ms,
      items: [{ rank: 1, path: "target.md", matched_by: "fts+vector" }],
    })),
  };
  success.quality_mean = summarizeRepeatedQuality(
    success.calls,
    success.targets,
  );
  const failed = {
    task_id: "b",
    mode: "hybrid",
    status: "failed",
    calls: [{ status: "failed", latency_ms: 1 }],
  };
  const summary = summarizePilotRows([success, failed], undefined, 20);
  assert.equal(summary[0].completed, 1);
  assert.equal(summary[0].metrics.file_hit_at_1, 1);
  assert.equal(summary[0].measurements.latency_sample_count, 5);
  assert.equal(summary[0].measurements.latency_ms_p50, 120);
  assert.equal(summary[0].measurements.latency_ms_mean, 120);
  assert.equal(summary[1].metrics.ndcg_at_10, null);
  success.calls[0].items = [];
  success.quality_mean = summarizeRepeatedQuality(
    success.calls,
    success.targets,
  );
  const unstable = summarizePilotRows([success, failed], undefined, 20);
  assert.equal(unstable[0].metrics.file_hit_at_1, 0.8);
  assert.equal(unstable[0].ranking_repeatable_cases, 0);
  assert.equal(unstable[0].measurements.latency_ms_mean, 120);
  const markdown = markdownPilotReport({
    label: "Pilot",
    status: "failed",
    model: "local/test",
    suite: "quarry20",
    summary: unstable,
    rows: [success, failed],
    failures: [{ task_id: "b", reason: "index failed" }],
  });
  assert.match(markdown, /1\/20/);
  assert.match(markdown, /index failed/);
  assert.match(markdown, /Per-query results/);
  assert.match(markdown, /\| a \| zg-hybrid \| ✅ Scored/);
  assert.match(markdown, /not the official Quarry function recall/);
});

test("a failed group audit removes every score and measurement produced by that group", () => {
  const rows = [
    {
      task_id: "a",
      mode: "fts",
      status: "success",
      reason: null,
      calls: [{ status: "success", latency_ms: 25, items: [] }],
      file: { hit_at_10: 1 },
      ndcg: { ndcg_at_10: 1 },
      quality_mean: { ndcg_at_10: 1 },
      output_bytes: 100,
      items: [],
    },
  ];
  invalidateGroupRows(rows, "group integrity was not verified: index changed");
  assert.equal(rows[0].status, "failed");
  assert.equal(rows[0].group_integrity, "failed");
  assert.match(rows[0].reason, /index changed/);
  assert.ok(!Object.hasOwn(rows[0], "file"));
  assert.ok(!Object.hasOwn(rows[0], "ndcg"));
  assert.ok(!Object.hasOwn(rows[0], "quality_mean"));
  assert.ok(!Object.hasOwn(rows[0], "output_bytes"));
  assert.ok(!Object.hasOwn(rows[0], "items"));
  assert.equal(rows[0].calls.length, 1, "raw call evidence remains available");
  const summary = summarizePilotRows(rows, ["fts"], 1);
  assert.equal(summary[0].measurements.latency_sample_count, 0);
  assert.equal(summary[0].measurements.latency_ms_mean, null);
  assert.equal(summary[0].measurements.latency_ms_p50, null);
  const markdown = markdownPilotReport({
    label: "Pilot",
    status: "failed",
    model: "local/test",
    suite: "quarry20",
    summary,
    rows,
    failures: [{ task_id: "a", reason: rows[0].reason }],
  });
  assert.match(markdown, /\| a \| zg-fts \|.*\| — \| — \|$/m);
});

test("unified results page identifies missing pilot artifacts independently", async () => {
  const result = await buildCombined({
    zg: null,
    beir: null,
    duretrieval: null,
    quarry: null,
    candidateCommit: "a".repeat(40),
  });
  assert.equal(result.status, "failed");
  assert.equal(result.pilots.beir.status, "unavailable");
  assert.equal(result.pilots.duretrieval.status, "unavailable");
  assert.equal(result.pilots.quarry.status, "unavailable");
  const markdown = markdownCombined(result);
  assert.match(markdown, /SWE-QA20/);
  assert.match(markdown, /BEIR \/ four datasets/);
  assert.match(markdown, /DuRetrieval \/ Chinese web search/);
  assert.match(markdown, /Quarry \/ eight languages/);
  assert.match(markdown, /report artifact missing/);
});

test("a failed pilot query keeps partial aggregate and all per-query conclusions", async () => {
  const pilot = await loadPilot("beir");
  const rows = pilot.lock.tasks.flatMap((task) => {
    const targets = task.qrels.map((item) => ({
      path: `docs/${item.document_id}.md`,
    }));
    return pilot.modes.map((mode) => ({
      task_id: task.id,
      mode,
      query: task.query,
      targets,
      calls: [],
      status: "failed",
      reason: "index failed",
    }));
  });
  const scored = rows[0];
  scored.status = "success";
  delete scored.reason;
  scored.calls = [1, 2, 3, 4, 5].map((repetition) => ({
    repetition,
    status: "success",
    latency_ms: 10,
    items: [],
  }));
  scored.items = [];
  scored.file = scoreFileRetrieval([], scored.targets);
  scored.ndcg = scoreNdcg([], scored.targets);
  scored.quality_mean = summarizeRepeatedQuality(scored.calls, scored.targets);
  scored.output_bytes = 0;
  const report = {
    schema_version: 2,
    protocol: PILOT_PROTOCOL,
    preview: PILOT_PREVIEW,
    quality_aggregation: "mean_of_five",
    suite: pilot.lock.suite,
    label: "BEIR / four datasets (test)",
    model: pilot.lock.model,
    candidate_commit: "a".repeat(40),
    status: "failed",
    rows,
    failures: [{ task_id: rows[1].task_id, reason: "index failed" }],
    summary: summarizePilotRows(rows, pilot.modes, pilot.lock.tasks.length),
    breakdown: summarizePilotBreakdown(pilot, rows),
  };
  const result = await buildCombined({
    zg: null,
    beir: report,
    duretrieval: null,
    quarry: null,
    candidateCommit: "a".repeat(40),
  });
  assert.equal(result.pilots.beir.status, "failed");
  assert.equal(result.pilots.beir.report.summary[0].completed, 1);
  const markdown = markdownCombined(result);
  assert.match(markdown, /BEIR \/ four datasets \| ❌ Incomplete \| 0\/20/);
  assert.match(markdown, /Per-query results \(all 20 queries × 3 modes\)/);
  assert.match(markdown, /\| scifact \| zg-hybrid \| 1\/10 \|/);
  assert.match(markdown, /\| nfcorpus \| zg-hybrid \| 0\/4 \|/);
  assert.match(markdown, /index failed/);
  for (const change of [
    { preview: "short" },
    { preview: undefined },
    { protocol: undefined },
    { protocol: "legacy-default" },
  ]) {
    const changed = await buildCombined({
      zg: null,
      beir: { ...report, ...change },
      duretrieval: null,
      quarry: null,
      candidateCommit: "a".repeat(40),
    });
    assert.equal(changed.pilots.beir.report, null);
    assert.match(changed.pilots.beir.error, /preview|protocol/);
  }
});

test("the shared parser accepts only one trailing empty Markdown line outside the public range", () => {
  const response = (last) => ({
    content: [
      {
        type: "text",
        text: `freshness: fresh\n#1 matchedBy=fts docs/1.md:1-3\nsource:\n1\tTitle\n2\t\n3\tBody\n4\t${last}\n`,
      },
    ],
  });
  const parsed = parseVisibleResponse(response(""));
  assert.equal(parsed.items[0].path, "docs/1.md");
  assert.throws(
    () => parseVisibleResponse(response("unexpected")),
    /outside the item's public range/,
  );
});

test("pilot Entity scoring distinguishes chunks on the same UTF-8 source line", () => {
  const path = "docs/positive.md";
  const text = "前缀 alpha beta tail";
  const file = { lines: [{ text, offset: 0 }] };
  const range = { kind: "text", start_line: 1, end_line: 1 };
  const hit = (rank, content) => ({
    path,
    rank,
    range,
    source_lines: [{ line: 1, text: content }],
  });
  const alpha = locateFullEntity(hit(1, "alpha"), file);
  assert.deepEqual(alpha, { start_byte: 7, end_byte: 12 });
  assert.equal(
    locateFullEntity(hit(1, "alpha"), {
      lines: [{ text: "alpha alpha", offset: 0 }],
    }),
    null,
  );

  const gold = {
    files: [{ path }],
    targets: [
      { path, source_id: "alpha", start_line: 1, end_line: 1, ...alpha },
      {
        path,
        source_id: "beta",
        start_line: 1,
        end_line: 1,
        start_byte: 13,
        end_byte: 17,
      },
    ],
  };
  const score = (items) =>
    summarizePilotEntity(
      [1, 2, 3, 4, 5].map((repetition) => ({
        repetition,
        items,
        status: "success",
      })),
      gold,
    );
  const withBytes = (item) => ({
    ...item,
    source_bytes: locateFullEntity(item, file),
  });
  const items = [
    withBytes(hit(1, "tail")),
    withBytes(hit(2, "alpha beta")),
    withBytes(hit(3, "alpha")),
  ];
  const scored = score(items);
  assert.equal(scored.file.recall_at_1, 1);
  assert.equal(scored.entity.recall_at_1, 0);
  assert.equal(scored.entity.recall_at_5, 1);
  assert.equal(scored.entity.rr_at_10, 1 / 2);
  assert.equal(scored.entity.ndcg_at_10, 1 / Math.log2(3));
  assert.deepEqual(
    score(items.map((item) => ({ ...item, source_lines: [] }))),
    scored,
  );
  const uncertain = score([hit(1, "al…")]);
  assert.equal(uncertain.entity.recall_at_1, null);
  assert.equal(uncertain.entity.scored_calls.recall_at_1, 0);
  assert.equal(uncertain.file.recall_at_1, 1);
  const split = score([withBytes(hit(1, "al")), withBytes(hit(2, "pha"))]);
  assert.equal(split.entity.recall_at_5, 0);
  assert.equal(
    summarizePilotEntity([], { ...gold, targets: [] }).reason,
    "no_verified_entity_evidence",
  );
});
