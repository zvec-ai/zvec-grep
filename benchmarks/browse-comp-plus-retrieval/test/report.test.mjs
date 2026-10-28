import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { auditArm, markdownReport, MODES, relativeChange } from "../report.mjs";
import { fileHash, writeJson } from "../../zg-retrieval/core/io.mjs";
import { parseVisibleResponse } from "../../zg-retrieval/engines/zg/parse.mjs";
import { statistics } from "../../zg-retrieval/metrics/statistics.mjs";
import { summarizeRepeatedQuality } from "../../zg-retrieval/metrics/repetitions.mjs";
import { summarizePerformance } from "../../zg-retrieval/reports/performance.mjs";

async function fixture(t) {
  const directory = await mkdtemp(
    join(tmpdir(), "browsecomp-retrieval-audit-"),
  );
  t.after(() => rm(directory, { recursive: true, force: true }));
  const task = {
    id: "1",
    query: "original fixture query",
    targets: [{ path: "1.md" }],
  };
  const dataset = { tasks: [task], corpus: { count: 1 } };
  const stage = () => ({
    status: "success",
    concurrency: 4,
    total_ms: 200,
    memory: {
      status: "available",
      samples: [{ elapsed_ms: 10, rss_bytes: 1024 }],
      rss_bytes: statistics([1024]),
    },
  });
  const group = {
    id: "full-corpus",
    index: { ...stage(), index_bytes: 2048 },
    query: {},
  };
  const arm = {
    corpus_root: "/fixture",
    corpus_before_sha256: "a".repeat(64),
    corpus_after_sha256: "a".repeat(64),
    model_before_sha256: "b".repeat(64),
    model_after_sha256: "b".repeat(64),
    rows: [],
    failures: [],
    performance_groups: [group],
  };
  for (const mode of MODES) {
    const row = {
      task_id: "1",
      mode,
      query: task.query,
      targets: task.targets,
      status: "success",
      group_integrity: "verified",
      calls: [],
    };
    const matched = mode === "hybrid" ? "fts+vector" : mode;
    for (let repetition = 1; repetition <= 5; repetition++) {
      const response = {
        content: [
          {
            type: "text",
            text: `freshness: fresh\n#1 matchedBy=${matched} 1.md:1\nsource:\n1\tfixture evidence`,
          },
        ],
      };
      const raw = join(
        directory,
        "evidence/full-corpus/raw",
        `1-${mode}-${repetition}.json`,
      );
      await writeJson(raw, {
        request: {
          name: "zvec_grep_search",
          arguments: {
            root: "/fixture",
            ...(mode === "hybrid"
              ? { query: task.query }
              : { [mode]: [task.query] }),
            limit: 10,
            autoUpdate: false,
            freshness: "eventual",
            preferSymbol: false,
          },
        },
        response,
      });
      row.calls.push({
        repetition,
        status: "success",
        latency_ms: repetition * 10,
        raw_sha256: await fileHash(raw),
        items: parseVisibleResponse(response).items.map(
          ({ rank, path, range, matched_by }) => ({
            rank,
            path,
            range,
            matched_by,
          }),
        ),
      });
    }
    row.quality_mean = summarizeRepeatedQuality(row.calls, task.targets);
    arm.rows.push(row);
    group.query[mode] = {
      ...stage(),
      failed_calls: 0,
      samples_ms: [10, 20, 30, 40, 50],
      latency_ms: statistics([10, 20, 30, 40, 50]),
    };
  }
  return { directory, arm, dataset };
}

test("saved MCP evidence rebuilds all quality and performance metrics", async (t) => {
  const { directory, arm, dataset } = await fixture(t);
  const result = await auditArm(arm, dataset, directory);
  assert.equal(result.calls, 15);
  assert.ok(result.quality.every((row) => row.metrics.file_hit_at_10 === 1));
  assert.equal(result.performance.query.hybrid.latency_ms.p50, 30);
  assert.equal(result.performance.query.fts.latency_ms.p99, 50);
  assert.equal(result.performance.index.index_bytes.total, 2048);
});

test("replay rejects rewritten queries, altered scores, missing RSS and substituted evidence", async (t) => {
  const { directory, arm, dataset } = await fixture(t);
  for (const mutate of [
    (a) => {
      a.rows[0].query = "rewritten";
    },
    (a) => {
      a.rows[0].quality_mean.file.hit_at_10 = 0;
    },
    (a) => {
      a.model_after_sha256 = "f".repeat(64);
    },
    (a) => {
      a.performance_groups[0].index.memory.status = "unavailable";
    },
    (a) => {
      a.performance_groups[0].query.hybrid.samples_ms[0] = 0;
    },
    (a) => {
      a.rows[0].calls[0].raw_sha256 = "f".repeat(64);
    },
    (a) => {
      a.rows.push(structuredClone(a.rows[0]));
    },
  ]) {
    const changed = structuredClone(arm);
    mutate(changed);
    await assert.rejects(auditArm(changed, dataset, directory));
  }
});

test("percentage reports retain increases, zero values and unavailable denominators", () => {
  assert.equal(relativeChange(100, 80), -20);
  assert.equal(relativeChange(100, 125), 25);
  assert.equal(relativeChange(100, 0), -100);
  assert.equal(relativeChange(0, 100), null);
  const p = summarizePerformance([]);
  const report = {
    status: "failed",
    rust_commit: "a".repeat(40),
    harness_commit: "b".repeat(40),
    dataset: { tasks: [], corpus: { count: 100195 } },
    arms: { node: { performance: p }, rust: { performance: p } },
    errors: ["incomplete"],
  };
  const text = markdownReport(report);
  assert.match(text, /Failed \/ incomplete/);
  assert.match(text, /request RT p50/);
  assert.match(text, /sampled RSS p99/);
  assert.match(text, /generated index total bytes/);
  assert.match(text, /concurrency limit 4/);
  assert.match(text, /one observation/);
  assert.match(text, /incomplete/);
});

test("BrowseComp workflow uses one runner, fresh indexes, fixed local embedding and manual routing", async () => {
  const workflow = await readFile(
    new URL(
      "../../../.github/workflows/browse-comp-plus-retrieval.yml",
      import.meta.url,
    ),
    "utf8",
  );
  assert.match(workflow, /RETRIEVAL_EMBEDDING: local/);
  assert.match(workflow, /Fresh indexes and paired retrieval on one runner/);
  assert.match(workflow, /--rust-commit/);
  assert.match(workflow, /--harness-commit/);
  assert.match(workflow, /contents: read/);
  assert.doesNotMatch(
    workflow,
    /secrets:|secrets\.|opencode|judge|parallel_cases|actions\/cache/,
  );
  const caller = await readFile(
    new URL("../../../.github/workflows/retrieval-pr.yml", import.meta.url),
    "utf8",
  );
  assert.match(caller, /inputs\.suite != 'browsecomp-plus'/);
  assert.match(
    caller,
    /github\.event_name == 'workflow_dispatch' && inputs\.suite == 'browsecomp-plus'/,
  );
});
