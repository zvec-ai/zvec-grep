import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { loadSuite, writeJson, fileHash } from "../core/lib.mjs";
import { requestArguments, callPlan } from "../engines/zg/run.mjs";
import {
  GRAPH_PAIR,
  graphCallPlan,
  auditGraphCalls,
  compareGraphReports,
  graphPairMarkdown,
} from "../engines/zg/graph-pair.mjs";
import {
  scoreQaRetrieval,
  summarizeRepeatedQaRetrieval,
  summarizeQaRetrieval,
} from "../metrics/entities.mjs";
import { summarizeRepeatedQuality } from "../metrics/repetitions.mjs";
import { scoreNdcg } from "../metrics/ndcg.mjs";
import {
  FILE_RETRIEVAL_CONTRACT,
  fileRetrievalForRow,
} from "../metrics/files.mjs";
import { summarizeMeasurements } from "../metrics/measurements.mjs";
import { validateZgReport } from "../reports/validation.mjs";

const suite = await loadSuite();
const task = suite.lock.tasks.find((task) => task.task_id === "xarray:46");
const commit = "a".repeat(40);

test("graph pairs alternate order, use ten results and preserve the original three-mode plan", () => {
  const plan = graphCallPlan([task], suite.protocol);
  assert.equal(plan.length, 10);
  assert.deepEqual(
    plan.map((call) => call.graph_rerank),
    [false, true, true, false, false, true, true, false, false, true],
  );
  for (const call of plan) {
    const args = requestArguments(
      task,
      "/corpus",
      "hybrid",
      suite.protocol,
      call.graph_rerank,
    );
    assert.equal(args.query, task.query);
    assert.equal(args.limit, 10);
    assert.equal(args.graphRerank, call.graph_rerank);
    assert.equal(args.trace, true);
  }
  assert.equal(callPlan([task], suite.protocol).length, 15);
  assert.equal(
    Object.hasOwn(
      requestArguments(task, "/corpus", "hybrid", suite.protocol),
      "graphRerank",
    ),
    false,
  );
  assert.throws(() => graphCallPlan([task], { ...suite.protocol, limit: 50 }));
});

test("graph pair audit rejects fallback, changed ordinary recall, tampering and incomplete pairs", async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "zg-graph-pair-test-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const manifest = {
    graph_pair: GRAPH_PAIR,
    tasks: [task.task_id],
    corpus_root: "/corpus",
  };
  const calls = [];
  for (const call of graphCallPlan([task], suite.protocol)) {
    const record = {
      ...call,
      task: undefined,
      task_id: task.task_id,
      request: {
        name: "zvec_grep_search",
        arguments: requestArguments(
          task,
          "/corpus",
          "hybrid",
          suite.protocol,
          call.graph_rerank,
        ),
      },
      raw_path: `raw/${task.task_slug}-${call.arm}-full-${call.repetition}.json`,
    };
    const response = {
      structuredContent: {
        searchTrace: [
          {
            recall: [],
            fused: [],
            ordinary_pool: [],
            graph: call.graph_rerank ? { error: null } : null,
          },
        ],
      },
    };
    await writeJson(join(directory, record.raw_path), response);
    record.raw_sha256 = await fileHash(join(directory, record.raw_path));
    calls.push(record);
  }
  await auditGraphCalls(directory, manifest, calls, suite);
  await assert.rejects(
    auditGraphCalls(directory, manifest, calls.slice(1), suite),
    /matrix/,
  );
  const changedRequest = structuredClone(calls);
  changedRequest[0].request.arguments.limit = 50;
  await assert.rejects(
    auditGraphCalls(directory, manifest, changedRequest, suite),
  );
  const on = calls.find((call) => call.graph_rerank);
  for (const graph of [null, { error: "storage failure" }]) {
    await writeJson(join(directory, on.raw_path), {
      structuredContent: {
        searchTrace: [{ recall: [], fused: [], ordinary_pool: [], graph }],
      },
    });
    on.raw_sha256 = await fileHash(join(directory, on.raw_path));
    await assert.rejects(
      auditGraphCalls(directory, manifest, calls, suite),
      /graph/,
    );
  }
  await writeJson(join(directory, on.raw_path), {
    structuredContent: {
      searchTrace: [
        {
          recall: [],
          fused: [],
          ordinary_pool: ["changed"],
          graph: { error: null },
        },
      ],
    },
  });
  on.raw_sha256 = await fileHash(join(directory, on.raw_path));
  await assert.rejects(
    auditGraphCalls(directory, manifest, calls, suite),
    /ordinary recall/,
  );
  on.raw_sha256 = "0".repeat(64);
  await assert.rejects(
    auditGraphCalls(directory, manifest, calls, suite),
    /response changed/,
  );
});

function armReport(enabled) {
  const targets = suite.file_gold[task.task_id].targets;
  const repeats = Array.from({ length: 5 }, (_, i) => ({
    repetition: i + 1,
    status: "scored",
    execution_status: "success",
    items: [],
    latency_ms: 1,
    visible_output_bytes: 11,
  }));
  const row = {
    task_id: task.task_id,
    repository: task.repository,
    category: task.category,
    language: suite.file_gold[task.task_id].language,
    gold_status: suite.gold[task.task_id].status,
    mode: "hybrid",
    preview: "full",
    repetition: 5,
    quality_observation: true,
    status: "scored",
    execution_status: "success",
    items: [],
    latency_ms: 1,
    visible_output_bytes: 11,
    ranking_repeatable: true,
    ndcg: { targets, ...scoreNdcg([], targets) },
    quality_observations: repeats.map(({ repetition, items }) => ({
      repetition,
      items,
    })),
    qa_observations: repeats,
    measurement_observations: repeats,
    quality_mean: summarizeRepeatedQuality(repeats, targets),
    qa_retrieval: scoreQaRetrieval([], suite.qa_pairs[task.task_id]),
    qa_quality_mean: summarizeRepeatedQaRetrieval(
      repeats,
      suite.qa_pairs[task.task_id],
    ),
  };
  row.file_retrieval = fileRetrievalForRow(row);
  return {
    schema_version: 7,
    file_retrieval_contract: FILE_RETRIEVAL_CONTRACT,
    preview: "full",
    quality_repetition: 5,
    quality_aggregation: "mean_of_five",
    suite: suite.identity,
    qa_gold: suite.qa_gold,
    qa_scoring: suite.qa_scoring,
    graph_rerank: enabled,
    scope: "explicit-subset",
    expected_task_ids: [task.task_id],
    observed_calls: 5,
    integrity_passed: true,
    quality_score_valid: true,
    integrity_errors: [],
    product_error_calls: 0,
    tasks: [row],
    modes: {
      hybrid: {
        qa_retrieval: summarizeQaRetrieval([row]),
        measurements: summarizeMeasurements(repeats),
      },
    },
    repositories: [
      {
        repository: task.repository,
        repository_commit: task.repository_commit,
        candidate_commit: commit,
        harness_commit: "e".repeat(40),
        corpus_root: "/corpus",
        graph_pair: GRAPH_PAIR,
        package: { tarball_sha256: "b".repeat(64) },
        index_content_sha256: "c".repeat(64),
        model_files_sha256: "d".repeat(64),
      },
    ],
  };
}

test("graph reports reuse Gold scoring and validation without weakening the default three-mode contract", () => {
  const off = armReport(false),
    on = armReport(true);
  assert.throws(
    () => validateZgReport(off, "ordinary", { suite }),
    /three modes/,
  );
  const result = compareGraphReports(off, on, {
    suite,
    candidateCommit: commit,
  });
  assert.equal(result.scopes.all.off.diagnostic.entity.recall_at_10, 0);
  assert.match(graphPairMarkdown(result), /graph-off.*Entity/);
  const corrupted = structuredClone(on);
  corrupted.tasks[0].qa_quality_mean.entity.recall_at_10 = 1;
  assert.throws(
    () =>
      compareGraphReports(off, corrupted, { suite, candidateCommit: commit }),
    /QA repeated scores/,
  );
  const otherIndex = structuredClone(on);
  otherIndex.repositories[0].index_content_sha256 = "f".repeat(64);
  assert.throws(
    () =>
      compareGraphReports(off, otherIndex, { suite, candidateCommit: commit }),
    /share the same indexes/,
  );
});
