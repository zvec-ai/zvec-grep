import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { readStageTrace } from "../../core/gold-stage-capture.mjs";
import { buildTraceData } from "../../reports/trace-data.mjs";
import { renderGoldTrace } from "../../reports/trace-html.mjs";
import { join } from "node:path";
import { readJson, fileHash } from "../../core/lib.mjs";
import { summarizeQaRetrieval } from "../../metrics/entities.mjs";
import { validateZgReport } from "../../reports/validation.mjs";
import { qaRetrievalMarkdown } from "../../reports/zg.mjs";

export const GRAPH_PAIR = Object.freeze({
  id: "sweqa20-head-graph-rerank-pair-v1",
  limit: 10,
  repetitions: 5,
  index: "one-fresh-graph-index-per-repository-shared-by-both-arms",
  order: "task-repetition-alternating-off-on",
});

export const FOCUS_TASKS = [
  "sqlfluff:2",
  "xarray:46",
  "sympy:38",
  "astropy:38",
  "matplotlib:37",
  "conan:19",
  "django:21",
];

export function graphCallPlan(tasks, protocol) {
  assert.equal(protocol.limit, GRAPH_PAIR.limit);
  assert.equal(protocol.repetitions, GRAPH_PAIR.repetitions);
  return tasks.flatMap((task) =>
    Array.from({ length: 5 }, (_, index) =>
      (index % 2 === 0 ? [false, true] : [true, false]).map((enabled) => ({
        task,
        mode: "hybrid",
        preview: "full",
        repetition: index + 1,
        quality_observation: index === 4,
        graph_rerank: enabled,
        arm: enabled ? "graph-on" : "graph-off",
      })),
    ).flat(),
  );
}

export async function auditGraphCalls(directory, manifest, calls, suite) {
  assert.deepEqual(manifest.graph_pair, GRAPH_PAIR, "wrong graph protocol");
  const tasks = manifest.tasks.map((id) =>
    suite.lock.tasks.find((task) => task.task_id === id),
  );
  assert.ok(tasks.every(Boolean), "unknown graph task");
  const plan = graphCallPlan(tasks, suite.protocol);
  assert.equal(calls.length, plan.length, "incomplete graph pair matrix");
  const pairs = new Map();
  for (const [index, call] of calls.entries()) {
    const expected = plan[index];
    for (const field of [
      "mode",
      "preview",
      "repetition",
      "quality_observation",
      "graph_rerank",
      "arm",
    ])
      assert.equal(
        call[field],
        expected[field],
        `graph call ${index}: wrong ${field}`,
      );
    assert.equal(call.task_id, expected.task.task_id);
    assert.deepEqual(call.request, {
      name: "zvec_grep_search",
      arguments: {
        root: manifest.corpus_root,
        query: expected.task.query,
        limit: 10,
        ...suite.protocol.request,
        trace: true,
        graphRerank: expected.graph_rerank,
      },
    });
    assert.equal(
      call.raw_path,
      `raw/${expected.task.task_slug}-${expected.arm}-full-${expected.repetition}.json`,
    );
    const path = join(directory, call.raw_path);
    assert.equal(
      await fileHash(path),
      call.raw_sha256,
      "graph response changed",
    );
    const response = await readJson(path);
    assert.notEqual(response.isError, true, "graph pair request failed");
    const groups = response.structuredContent?.searchTrace;
    assert.equal(groups?.length, 1, "graph experiment requires query trace");
    const trace = groups[0];
    if (call.graph_rerank) {
      assert.ok(
        trace.graph,
        "graphRerank did not run: graph index or option missing",
      );
      assert.equal(trace.graph.error, null, "graph expansion failed");
    } else assert.equal(trace.graph, null, "disabled arm expanded the graph");
    const identity = `${call.task_id}/${call.repetition}`;
    const ordinary = {
      recall: trace.recall,
      fused: trace.fused,
      ordinary_pool: trace.ordinary_pool,
    };
    if (manifest.gold_stages) {
      const capture = readStageTrace(response);
      assert.ok(
        capture && !(capture instanceof Error),
        "missing graph stage capture",
      );
      assert.equal(
        capture.index_observed,
        true,
        "graph index stage unavailable",
      );
      assert.equal(capture.limit, 10, "wrong stage limit");
      // Final ranks are the experimental outcome. All earlier observations must agree.
      ordinary.stages = {
        routes: capture.routes,
        files: Object.fromEntries(
          Object.entries(capture.files).map(([path, entities]) => [
            path,
            Object.fromEntries(
              Object.entries(entities).map(([id, entity]) => {
                const { final_rank, ...beforeRanking } = entity;
                return [id, beforeRanking];
              }),
            ),
          ]),
        ),
      };
    }
    if (pairs.has(identity))
      assert.deepEqual(
        ordinary,
        pairs.get(identity),
        "graph pair changed ordinary recall",
      );
    else pairs.set(identity, ordinary);
  }
}

export function compareGraphReports(off, on, { suite, candidateCommit }) {
  assert.match(
    candidateCommit,
    /^[a-f0-9]{40}$/,
    "graph experiment requires an exact candidate commit",
  );
  for (const [arm, report] of Object.entries({ off, on })) {
    validateZgReport(report, `graph-${arm}`, { suite, graphPair: true });
    assert.equal(
      report.integrity_passed,
      true,
      `graph-${arm}: failed experiment`,
    );
    assert.equal(report.graph_rerank, arm === "on");
    for (const repository of report.repositories) {
      assert.equal(
        repository.candidate_commit,
        candidateCommit,
        "wrong graph candidate",
      );
      assert.match(repository.harness_commit, /^[a-f0-9]{40}$/);
      assert.equal(
        repository.harness_commit,
        off.repositories[0].harness_commit,
      );
      assert.deepEqual(repository.graph_pair, GRAPH_PAIR);
    }
  }
  assert.deepEqual(off.expected_task_ids, on.expected_task_ids);
  const identities = (report) =>
    report.repositories.map((repo) => ({
      repository: repo.repository,
      commit: repo.repository_commit,
      root: repo.corpus_root,
      index: repo.index_content_sha256,
      model: repo.model_files_sha256,
      package: repo.package.tarball_sha256,
    }));
  assert.deepEqual(
    identities(off),
    identities(on),
    "graph arms must share the same indexes and runtime",
  );
  const scopes = Object.fromEntries(
    [
      ["all", off.expected_task_ids],
      ["focus", off.expected_task_ids.filter((id) => FOCUS_TASKS.includes(id))],
    ].map(([name, ids]) => [
      name,
      {
        task_ids: ids,
        off: summarizeQaRetrieval(
          off.tasks.filter((row) => ids.includes(row.task_id)),
        ),
        on: summarizeQaRetrieval(
          on.tasks.filter((row) => ids.includes(row.task_id)),
        ),
      },
    ]),
  );
  return {
    schema_version: 1,
    kind: "head-graph-rerank-comparison",
    status: "success",
    candidate_commit: candidateCommit,
    harness_commit: off.repositories[0].harness_commit,
    protocol: GRAPH_PAIR,
    suite: suite.identity,
    qa_gold: suite.qa_gold,
    qa_scoring: suite.qa_scoring,
    paired_calls: off.expected_task_ids.length * 5,
    indexes: identities(off),
    scopes,
    measurements: {
      off: off.modes.hybrid.measurements,
      on: on.modes.hybrid.measurements,
    },
  };
}

export function graphPairMarkdown(result) {
  const lines = [
    "### Head graphRerank: off → on",
    "",
    "Same Head binary, graph index, original questions and model; limit 10, five paired repetitions. Gold and containment scoring are unchanged.",
  ];
  for (const [name, scope] of Object.entries(result.scopes)) {
    lines.push(
      "",
      `**${name === "all" ? "SWE-QA20" : "Seven focus cases"}**`,
      ...qaRetrievalMarkdown(
        [
          ["graph-off", scope.off],
          ["graph-on", scope.on],
        ],
        scope.task_ids.length,
      ),
    );
  }
  lines.push(
    "",
    "| Arm | Avg RT (ms) | P50 RT (ms) |",
    "| --- | ---: | ---: |",
  );
  for (const arm of ["off", "on"])
    lines.push(
      `| graph-${arm} | ${result.measurements[arm].latency_ms_mean.toFixed(2)} | ${result.measurements[arm].latency_ms_p50.toFixed(2)} |`,
    );
  lines.push(
    "",
    "Latency includes trace collection and fixed-session cache effects; it is not a controlled speed comparison.",
  );
  return lines.join("\n") + "\n";
}

export async function writeGraphTrace(directory, off, on, metadata) {
  const sides = await Promise.all(
    ["off", "on"].map(async (arm, index) => {
      const trace = await readJson(
        join(directory, `graph-${arm}-gold-stages.json`),
      );
      assert.ok(
        trace.calls.length > 0 &&
          trace.calls.every((call) => call.state === "observed"),
        `graph-${arm} has incomplete stage evidence`,
      );
      return { reports: { zg: index === 0 ? off : on }, traces: { zg: trace } };
    }),
  );
  const data = await buildTraceData({
    baseline: sides[0],
    candidate: sides[1],
    metadata,
    graphPair: true,
  });
  for (const suite of data.suites)
    for (const side of [suite.baseline, suite.candidate])
      assert.equal(side.error, undefined, "invalid graph stage report");
  await writeFile(
    join(directory, "graph-rerank-trace.html"),
    await renderGoldTrace(data),
  );
}
