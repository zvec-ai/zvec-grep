import assert from "node:assert/strict";
import { loadSuite } from "../core/suite.mjs";
import { PILOT_NAMES, PILOT_SUITES } from "../expansion/config.mjs";
import { loadPilot } from "../expansion/datasets.mjs";
import { GOLD_STAGES, summarizeGoldStages } from "../metrics/gold-stages.mjs";
import { validateGoldStageSummary } from "./gold-stages.mjs";

const MODES = ["fts", "vector", "hybrid"];

function cachedSummary(side, name) {
  return (
    side?.reports?.[name]?.gold_stages ??
    (name === "zg"
      ? side?.summary?.sweqa?.gold_stages
      : side?.summary?.pilots?.[name]?.report?.gold_stages) ??
    null
  );
}

function validateCalls(calls, tasks, repetitions) {
  assert.ok(Array.isArray(calls), "Stage calls must be an array");
  const known = new Set(tasks.map(({ id }) => id));
  const groups = new Map();
  for (const call of calls) {
    assert.ok(known.has(call.task_id), "Unknown stage task");
    assert.ok(MODES.includes(call.mode), "Unknown stage mode");
    assert.ok(
      ["formal", "diagnostic", "unavailable"].includes(call.qualification),
      "Unknown stage qualification",
    );
    assert.ok(
      ["observed", "unobserved", "index_unobserved", "invalid"].includes(
        call.state,
      ),
      "Unknown stage capture state",
    );
    const key = `${call.task_id}/${call.mode}`;
    const rows = groups.get(key) ?? [];
    rows.push(call.repetition);
    groups.set(key, rows);
    assert.ok(Array.isArray(call.targets), "Missing stage targets");
    const ids = new Set();
    for (const target of call.targets) {
      assert.ok(
        ["file", "entity"].includes(target.level),
        "Invalid target level",
      );
      assert.ok(
        typeof target.id === "string" && target.id,
        "Missing target ID",
      );
      const id = `${target.level}/${target.id}`;
      assert.ok(!ids.has(id), "Duplicate stage target");
      ids.add(id);
      for (const { id: stage } of GOLD_STAGES) {
        if (
          (stage === "fts" && call.mode === "vector") ||
          (stage === "vector" && call.mode === "fts")
        )
          continue;
        assert.ok(
          [true, false, null].includes(target.coverage?.[stage]),
          "Missing or invalid stage coverage",
        );
      }
    }
  }
  const expected = Array.from({ length: repetitions }, (_, index) => index + 1);
  for (const task of tasks)
    for (const mode of MODES)
      assert.ok(
        groups.has(`${task.id}/${mode}`),
        "Incomplete stage task/mode matrix",
      );
  for (const rows of groups.values())
    assert.deepEqual(
      rows.sort((a, b) => a - b),
      expected,
      "Incomplete or duplicate stage repetitions",
    );
}

function buildSide(side, name, definitions, repetitions) {
  const tasks = definitions.map((task) => ({
    ...task,
    variants: {},
    groups: {},
  }));
  let summary = null;
  try {
    const report = side?.reports?.[name];
    for (const row of report?.[name === "zg" ? "tasks" : "rows"] ?? []) {
      const task = tasks.find(({ id }) => id === row.task_id);
      assert.ok(task, "Unknown report task");
      const query = name === "zg" ? row.request?.arguments?.query : row.query;
      if (query !== undefined)
        assert.equal(
          query,
          task.query,
          "Report query differs from original query",
        );
      if (row.corpus_id) task.corpus = row.corpus_id;
    }
    const trace = side?.traces?.[name];
    if (!trace) {
      const cached = cachedSummary(side, name);
      if (cached) {
        validateGoldStageSummary(cached);
        summary = cached;
      }
      return { summary, tasks };
    }
    validateCalls(trace.calls, tasks, repetitions);
    const rebuilt = summarizeGoldStages(trace.calls);
    validateGoldStageSummary(rebuilt);
    if (trace.summary)
      assert.deepEqual(
        validateGoldStageSummary(trace.summary),
        validateGoldStageSummary(rebuilt),
        "Cached stage summary differs from per-call evidence",
      );
    summary = rebuilt;
    for (const task of tasks) {
      const calls = trace.calls.filter(({ task_id }) => task_id === task.id);
      if (!calls.length) continue;
      const qualifications = new Set(
        calls.map(({ qualification }) => qualification),
      );
      assert.equal(
        qualifications.size,
        1,
        "Task qualification changed between calls",
      );
      task.qualification = calls[0].qualification;
      if (task.qualification === "unavailable")
        task.excludedReason = "Gold is not eligible for aggregate coverage.";
      // Ineligible questions retain their own trace without entering suite totals.
      const groups = summarizeGoldStages(
        calls.map((call) => ({ ...call, qualification: "diagnostic" })),
      ).groups;
      for (const group of groups) {
        task.groups[group.mode] ??= {};
        task.groups[group.mode][group.level] = group;
      }
      for (const call of calls.sort((a, b) => a.repetition - b.repetition)) {
        task.variants[call.mode] ??= [];
        task.variants[call.mode].push({
          repetitions: [call.repetition],
          state: call.state,
          error: call.error,
          sizes: call.sizes,
          routes: call.routes,
          targets: call.targets,
        });
      }
    }
    return { summary, tasks };
  } catch (error) {
    // A diagnostic failure must not change retrieval quality or imply a miss.
    return {
      summary: null,
      tasks: tasks.map((task) => ({ ...task, variants: {}, groups: {} })),
      error: `Stage evidence unavailable: ${error.message}`,
    };
  }
}

export async function buildTraceData({ baseline, candidate, metadata = {} }) {
  const suite = await loadSuite();
  const definitions = [
    {
      name: "zg",
      id: "sweqa",
      label: "SWE-QA20",
      tasks: suite.lock.tasks.map((task) => {
        const pair = suite.qa_pairs[task.task_id];
        return {
          id: task.task_id,
          query: task.query,
          corpus: task.repository,
          qualification: pair.scoring_eligible
            ? "formal"
            : pair.diagnostic_eligible
              ? "diagnostic"
              : "unavailable",
        };
      }),
    },
    ...(await Promise.all(
      PILOT_NAMES.map(async (name) => {
        const pilot = await loadPilot(name);
        return {
          name,
          id: name,
          label: PILOT_SUITES[name].report.comparison,
          tasks: pilot.lock.tasks.map((task) => ({
            id: task.id,
            query: task.query,
            corpus: task.repository ?? task.dataset ?? name,
            qualification: "diagnostic",
          })),
        };
      }),
    )),
  ];
  return {
    meta: {
      ...metadata,
      limit: suite.protocol.limit,
      repetitions: suite.protocol.repetitions,
    },
    suites: definitions.map(({ name, id, label, tasks }) => ({
      id,
      label,
      baseline: buildSide(baseline, name, tasks, suite.protocol.repetitions),
      candidate: buildSide(candidate, name, tasks, suite.protocol.repetitions),
    })),
  };
}
