import assert from "node:assert/strict";
import test from "node:test";
import { loadSuite } from "../core/suite.mjs";
import { loadPilot } from "../expansion/datasets.mjs";
import { GOLD_STAGES, summarizeGoldStages } from "../metrics/gold-stages.mjs";
import { buildTraceData } from "../reports/trace-data.mjs";

const suite = await loadSuite();
const task = suite.lock.tasks[0];
const makeCalls = (taskId = task.task_id, qualification = "diagnostic") =>
  Array.from({ length: 5 }, (_, index) => ({
    task_id: taskId,
    mode: "hybrid",
    repetition: index + 1,
    qualification,
    state: "observed",
    index_read_ms: 1,
    sizes: { merged: 200, fused: 200, loaded: 200, selected: 10 },
    routes: {
      fts: {
        attempts: [{ depth: 200, returned_fragments: 200 }],
        unique_fragments: 200,
      },
      vector: {
        attempts: [{ depth: 200, returned_fragments: 200 }],
        unique_fragments: 200,
      },
    },
    targets: ["file", "entity"].map((level) => ({
      id: `${level}-one`,
      level,
      path: "source.py",
      coverage: Object.fromEntries(
        GOLD_STAGES.map(({ id }) => [id, id !== "actual_top10" || index < 3]),
      ),
      ranks: { loaded: 2, static: 3, selected: index < 3 ? 5 : "miss" },
    })),
  }));
const input = (selected) => {
  const calls = suite.lock.tasks.flatMap(({ task_id }) =>
    ["fts", "vector", "hybrid"].flatMap((mode) => {
      const specified = selected.filter(
        (call) => call.task_id === task_id && call.mode === mode,
      );
      return specified.length
        ? specified
        : makeCalls(
            task_id,
            selected.find((call) => call.task_id === task_id)?.qualification,
          ).map((call) => ({
            ...call,
            mode,
            state: "unobserved",
            targets: [],
          }));
    }),
  );
  return {
    reports: {
      zg: {
        tasks: [
          {
            task_id: task.task_id,
            request: { arguments: { query: task.query } },
          },
        ],
      },
    },
    traces: { zg: { summary: summarizeGoldStages(calls), calls } },
  };
};

test("trace data preserves per-question means, native ranks, and separate baseline/candidate evidence", async () => {
  const before = makeCalls();
  const after = structuredClone(before);
  for (const call of after) call.targets[1].coverage.actual_top10 = true;
  const baseline = input(before),
    candidate = input(after);
  const original = structuredClone({ baseline, candidate });
  const data = await buildTraceData({ baseline, candidate });
  const result = data.suites[0];
  assert.deepEqual(
    result.baseline.summary,
    summarizeGoldStages(baseline.traces.zg.calls),
  );
  assert.deepEqual(
    result.candidate.summary,
    summarizeGoldStages(candidate.traces.zg.calls),
  );
  const b = result.baseline.tasks.find(({ id }) => id === task.task_id);
  const c = result.candidate.tasks.find(({ id }) => id === task.task_id);
  assert.equal(b.groups.hybrid.entity.targets, 1);
  assert.equal(b.groups.hybrid.entity.stages.actual_top10.covered, 0.6);
  assert.equal(c.groups.hybrid.entity.stages.actual_top10.covered, 1);
  assert.equal(b.variants.hybrid[4].targets[1].ranks.selected, "miss");
  assert.deepEqual(
    b.variants.hybrid.flatMap(({ repetitions }) => repetitions),
    [1, 2, 3, 4, 5],
  );
  assert.equal(b.query, task.query);
  assert.equal(data.meta.limit, suite.protocol.limit);
  assert.equal(data.meta.repetitions, 5);
  assert.deepEqual({ baseline, candidate }, original);
});

test("missing captures keep original questions and cached summaries without inventing per-call data", async () => {
  const summary = summarizeGoldStages(makeCalls());
  const data = await buildTraceData({
    candidate: { reports: { zg: { gold_stages: summary } } },
  });
  assert.deepEqual(data.suites[0].candidate.summary, summary);
  assert.equal(data.suites[0].baseline.summary, null);
  for (const side of ["baseline", "candidate"])
    for (const task of data.suites[0][side].tasks) {
      assert.deepEqual(task.variants, {});
      assert.deepEqual(task.groups, {});
    }
  const pilot = await loadPilot("duretrieval");
  const chinese = data.suites.find(({ id }) => id === "duretrieval");
  assert.equal(chinese.candidate.tasks[0].query, pilot.lock.tasks[0].query);
  assert.match(chinese.candidate.tasks[0].query, /[\u4e00-\u9fff]/);
});

test("ineligible questions keep traces but do not enter aggregate Gold counts", async () => {
  const calls = [...makeCalls(), ...makeCalls("streamlink:14", "unavailable")];
  const result = (await buildTraceData({ candidate: input(calls) })).suites[0]
    .candidate;
  assert.equal(
    result.summary.groups.find(
      ({ level, mode }) => level === "entity" && mode === "hybrid",
    ).targets,
    1,
  );
  const excluded = result.tasks.find(({ id }) => id === "streamlink:14");
  assert.equal(excluded.qualification, "unavailable");
  assert.equal(excluded.groups.hybrid.entity.targets, 1);
  assert.equal(excluded.variants.hybrid.length, 5);
});

test("unknown coverage and absent Entity annotations remain distinct from zero hits", async () => {
  const calls = makeCalls();
  for (const call of calls)
    call.targets = call.targets.filter(({ level }) => level === "file");
  calls[0].targets[0].coverage.actual_top10 = null;
  const data = await buildTraceData({ candidate: input(calls) });
  const groups = data.suites[0].candidate.tasks[0].groups.hybrid;
  assert.equal(groups.entity.targets, 0);
  assert.equal(groups.file.targets, 1);
  assert.equal(groups.file.stages.actual_top10.unknown, 0.2);
  assert.equal(groups.file.stages.actual_top10.covered, 0.4);
});

test("incomplete repetitions, malformed coverage, stale summaries, and rewritten queries report unavailable evidence", async () => {
  const changes = [
    (side) => side.traces.zg.calls.pop(),
    (side) => {
      side.traces.zg.calls = side.traces.zg.calls.filter(
        (call) => call.task_id !== task.task_id || call.mode !== "vector",
      );
      side.traces.zg.summary = summarizeGoldStages(side.traces.zg.calls);
    },
    (side) =>
      delete side.traces.zg.calls.find((call) => call.targets.length).targets[0]
        .coverage.fts,
    (side) => {
      side.traces.zg.summary.groups.find(
        ({ mode, level }) => mode === "hybrid" && level === "entity",
      ).stages.actual_top10.covered = 0;
    },
    (side) => {
      side.reports.zg.tasks[0].request.arguments.query = "rewritten";
    },
  ];
  for (const change of changes) {
    const candidate = input(makeCalls());
    change(candidate);
    const result = (await buildTraceData({ candidate })).suites[0].candidate;
    assert.match(result.error, /^Stage evidence unavailable:/);
    assert.equal(result.summary, null);
    assert.ok(
      result.tasks.every(
        ({ variants, groups }) =>
          !Object.keys(variants).length && !Object.keys(groups).length,
      ),
    );
  }
});
