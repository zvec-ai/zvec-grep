import assert from "node:assert/strict";
import test from "node:test";
import {
  stageTraceEnvironment,
  readStageTrace,
} from "../core/gold-stage-capture.mjs";
import { indexedSourceOffsets } from "../expansion/gold.mjs";
import { entityMatch } from "../metrics/entities.mjs";
import {
  scoreGoldStages,
  summarizeGoldStages,
} from "../metrics/gold-stages.mjs";
import { goldStagesMarkdown } from "../reports/gold-stages.mjs";

const path = "code.go";
const pair = {
  file_targets: [{ id: "file", path }],
  entity_targets: [
    {
      id: "first",
      path,
      start_line: 1,
      end_line: 2,
      start_byte: 1,
      end_byte: 22,
    },
    {
      id: "second",
      path,
      start_line: 9,
      end_line: 9,
      start_byte: 90,
      end_byte: 95,
    },
    {
      id: "third",
      path,
      start_line: 15,
      end_line: 16,
      start_byte: 141,
      end_byte: 166,
    },
  ],
};
const text = (first, last, start, end) => ({
  text: {
    start: { line: first, byte_offset: start },
    end: { line: last, byte_offset: end },
  },
});

function candidate(range, stages) {
  return {
    range,
    searchable: true,
    recall: {},
    merged: true,
    rrf_rank: null,
    loaded_rrf_rank: null,
    loaded_range: null,
    static_rank: null,
    final_rank: null,
    ...stages,
  };
}

function fixture() {
  const first = text(1, 2, 1, 22);
  const second = text(9, 9, 89, 98);
  const capture = {
    version: 1,
    group_id: "Q1",
    limit: 10,
    index_observed: true,
    index_read_ms: 2,
    watched_paths: [path],
    routes: { fts: {}, vector: {} },
    sizes: { merged: 12, fused: 12, loaded: 11, selected: 10 },
    files: {
      [path]: {
        a: candidate(first, {
          recall: { fts: 200, vector: 5 },
          rrf_rank: 2,
          loaded_rrf_rank: 2,
          loaded_range: first,
          static_rank: 11,
          final_rank: 5,
        }),
        b: candidate(second, {
          recall: { fts: 92 },
          rrf_rank: 11,
          loaded_rrf_rank: 11,
          loaded_range: second,
          static_rank: 1,
        }),
        c: candidate(text(15, 16, 140, 167), {
          recall: { vector: 16 },
          rrf_rank: 12,
        }),
      },
    },
  };
  const items = Array.from({ length: 10 }, (_, i) => ({
    rank: i + 1,
    path: `other-${i}.go`,
  }));
  items[4] = {
    rank: 5,
    path,
    range: { kind: "text", start_line: 1, end_line: 2 },
    source_bytes: { start_byte: 1, end_byte: 22 },
  };
  return {
    capture,
    pair,
    items,
    taskId: "task",
    mode: "hybrid",
    repetition: 1,
    qualification: "diagnostic",
    useBytes: true,
  };
}

test("compares loaded RRF and actual top ten without applying static rules to off", () => {
  const scored = scoreGoldStages(fixture());
  assert.equal(scored.state, "observed");
  const [file, first, second, third] = scored.targets;
  assert.equal(file.coverage.selected, true);
  assert.deepEqual(first.ranks, {
    fts: 200,
    vector: 5,
    fused: 2,
    loaded: 2,
    selected: 5,
    static: 11,
  });
  assert.equal(first.coverage.rrf_top10, true);
  assert.equal(first.coverage.actual_top10, true);
  assert.equal(second.coverage.index, true);
  assert.equal(second.coverage.vector, false);
  assert.equal(second.coverage.rrf_top10, false);
  assert.equal(second.coverage.actual_top10, false);
  assert.equal(second.ranks.selected, "miss");
  assert.equal(third.coverage.fused, true);
  assert.equal(third.coverage.loaded, false);
  const entity = summarizeGoldStages([scored]).groups.find(
    ({ level }) => level === "entity",
  );
  assert.equal(entity.targets, 3);
  assert.deepEqual(entity.stages.loaded, {
    covered: 2,
    unknown: 0,
    lost: 1,
    loss_unknown: 0,
  });
  assert.equal(entity.stages.rrf_top10.lost, 1);
  assert.equal(entity.stages.actual_top10.lost, 1);
});

test("off uses the loaded pool's RRF cutoff, excluding unloaded fusion hits", () => {
  const observed = fixture();
  const { a, b, c } = observed.capture.files[path];
  Object.assign(a, { rrf_rank: 11, loaded_rrf_rank: 10 });
  Object.assign(b, { rrf_rank: 12, loaded_rrf_rank: 11 });
  c.rrf_rank = 1;
  const scored = scoreGoldStages(observed);
  assert.equal(scored.state, "observed");
  const [file, first, second, third] = scored.targets;
  assert.equal(file.coverage.rrf_top10, true);
  assert.equal(first.ranks.fused, 11);
  assert.equal(first.ranks.loaded, 10);
  assert.equal(first.ranks.static, 11);
  assert.equal(first.coverage.rrf_top10, true);
  assert.equal(second.ranks.loaded, 11);
  assert.equal(second.ranks.static, 1);
  assert.equal(second.coverage.rrf_top10, false);
  assert.equal(third.ranks.fused, 1);
  assert.equal(third.coverage.rrf_top10, false);
  assert.deepEqual(
    scored.targets.map(({ coverage }) => coverage.actual_top10),
    scoreGoldStages(fixture()).targets.map(
      ({ coverage }) => coverage.actual_top10,
    ),
  );
});

test("keeps native loaded ranks when several Entities share a Gold file", () => {
  const observed = fixture();
  observed.capture.sizes = { merged: 13, fused: 13, loaded: 12, selected: 10 };
  const { a, b, c } = observed.capture.files[path];
  Object.assign(a, { rrf_rank: 12, loaded_rrf_rank: 11, static_rank: 2 });
  Object.assign(b, { rrf_rank: 13, loaded_rrf_rank: 12 });
  c.rrf_rank = 1;
  const scored = scoreGoldStages(observed);
  assert.equal(scored.state, "observed");
  assert.equal(scored.targets[0].coverage.rrf_top10, false);
  assert.equal(scored.targets[0].coverage.actual_top10, true);
  assert.equal(scored.targets[1].coverage.rrf_top10, false);
  assert.equal(scored.targets[1].coverage.actual_top10, true);
});

test("off preserves unknown Entity coverage only inside the RRF Top 10", () => {
  const observed = fixture();
  observed.useBytes = false;
  observed.capture.files[path].b.loaded_rrf_rank = 10;
  const scored = scoreGoldStages(observed);
  assert.equal(scored.state, "observed");
  assert.equal(scored.targets[2].coverage.rrf_top10, null);
  observed.capture.files[path].b.loaded_rrf_rank = 11;
  assert.equal(scoreGoldStages(observed).targets[2].coverage.rrf_top10, false);
});

test("keeps missing traces and ambiguous Entity ranges unknown", () => {
  const evidence = fixture();
  const unobserved = scoreGoldStages({ ...evidence, capture: null });
  assert.equal(unobserved.targets[1].coverage.index, null);
  assert.equal(unobserved.targets[1].ranks.selected, "unobserved");
  const lineOnly = scoreGoldStages({ ...evidence, useBytes: false });
  assert.equal(lineOnly.targets[2].coverage.index, null);
  evidence.items[4].path = "wrong.go";
  const mismatch = scoreGoldStages(evidence);
  assert.equal(mismatch.state, "invalid");
  assert.equal(mismatch.targets[1].coverage.selected, null);
});

test("SciFact Gold completes an alternative evidence group within each stage", () => {
  const observed = fixture();
  const [first, second, third] = pair.entity_targets;
  observed.pair = {
    ...pair,
    entity_targets: [
      {
        id: "scifact-document",
        path,
        evidence_sets: [
          { id: "both", spans: [first, second] },
          { id: "alternative", spans: [third] },
        ],
      },
    ],
  };
  const { a, b, c } = observed.capture.files[path];
  a.static_rank = 2;
  const score = () => scoreGoldStages(observed).targets[1];
  const complete = score();
  assert.equal(complete.ranks.fts, 200);
  assert.equal(complete.ranks.vector, 16);
  assert.equal(complete.ranks.fused, 11);
  assert.equal(complete.coverage.rrf_top10, false);
  assert.equal(complete.coverage.actual_top10, false);
  b.loaded_rrf_rank = 10;
  assert.equal(score().coverage.rrf_top10, true);
  assert.equal(score().coverage.actual_top10, false);

  // In this replay the required spans arrive through different routes.
  a.recall = { vector: 5 };
  c.recall = {};
  const split = score();
  assert.equal(split.coverage.fts, false);
  assert.equal(split.coverage.vector, false);
  assert.equal(split.coverage.recall, true);
  a.range = null;
  assert.equal(score().coverage.recall, null);

  observed.pair.entity_targets = [];
  assert.deepEqual(
    scoreGoldStages(observed).targets.map(({ level }) => level),
    ["file"],
  );
});

test("stage summary shows coverage at every stage", () => {
  const observed = fixture();
  observed.capture.files[path].a.static_rank = 2;
  const summary = summarizeGoldStages([
    scoreGoldStages(observed),
    scoreGoldStages({ ...fixture(), mode: "fts", capture: null }),
  ]);
  const lines = goldStagesMarkdown(summary);
  const entityStart = lines.indexOf("#### Entity Gold (diagnostic)");
  assert.ok(lines.includes("#### File Gold (diagnostic)"));
  assert.deepEqual(lines.slice(entityStart + 4, -1), [
    "| Gold covered by index | N/A | — | 3/3 |",
    "| Gold recalled by FTS | N/A | — | 2/3 |",
    "| Gold recalled by Vector | — | — | 2/3 |",
    "| Top 10 before rule ranking (RRF) | N/A | — | 1/3 |",
    "| Top 10 after rule ranking (actual) | N/A | — | 1/3 |",
  ]);
});

test("the standalone report rejects legacy static-only coverage", () => {
  const summary = summarizeGoldStages([scoreGoldStages(fixture())]);
  for (const { stages } of summary.groups) {
    stages.static_top10 = stages.rrf_top10;
    delete stages.rrf_top10;
  }
  assert.throws(
    () => goldStagesMarkdown(summary),
    /Invalid Gold stage coverage/,
  );
});

test("maps indexed offsets to frozen source bytes for a verified UTF-8 BOM", () => {
  const evidence = fixture();
  evidence.pair = {
    ...pair,
    entity_targets: pair.entity_targets.map((target) => ({
      ...target,
      start_byte: target.start_byte + 3,
      end_byte: target.end_byte + 3,
    })),
  };
  evidence.items[4].source_bytes = { start_byte: 4, end_byte: 25 };
  evidence.sourceOffsets = indexedSourceOffsets(
    new Map([
      [path, { bytes: Buffer.from("\uFEFFpackage example") }],
      ["plain.go", { bytes: Buffer.from("package example") }],
    ]),
  );
  assert.deepEqual(evidence.sourceOffsets, { [path]: 3 });
  const scored = scoreGoldStages(evidence);
  assert.equal(scored.state, "observed");
  assert.equal(scored.targets[1].coverage.index, true);
  assert.equal(scored.targets[1].ranks.selected, 5);
});

test("the compact public range may omit the final line separator", () => {
  const observed = fixture();
  const end = observed.capture.files[path].a.loaded_range.text.end;
  Object.assign(end, { line: 3, byte_column: 0, byte_offset: 23 });
  assert.equal(scoreGoldStages(observed).state, "observed");
  end.byte_offset = 24;
  assert.equal(scoreGoldStages(observed).state, "observed");
  end.byte_offset = 25;
  assert.equal(scoreGoldStages(observed).state, "invalid");
  end.byte_offset = 23;
  end.byte_column = 1;
  assert.equal(scoreGoldStages(observed).state, "invalid");
});

test("half-open stage ranges do not claim the following Gold line", () => {
  const observed = fixture();
  observed.useBytes = false;
  const covered = { id: "covered", path, start_line: 3, end_line: 4 };
  const missing = { id: "missing", path, start_line: 3, end_line: 5 };
  const boundary = { id: "boundary", path, start_line: 4, end_line: 4 };
  observed.pair = {
    file_targets: [],
    entity_targets: [covered, missing, boundary],
  };
  const range = text(3, 5, 4, 8);
  range.text.end.byte_column = 0;
  Object.assign(observed.capture.files[path].a, {
    range,
    loaded_range: range,
    static_rank: 5,
  });
  observed.items[4].range = { kind: "text", start_line: 3, end_line: 4 };
  const scored = scoreGoldStages(observed);
  assert.equal(scored.state, "observed");
  assert.ok(
    Object.values(scored.targets[0].coverage).every((value) => value === true),
  );
  assert.ok(
    Object.values(scored.targets[1].coverage).every((value) => value === false),
  );
  assert.ok(
    Object.values(scored.targets[2].coverage).every((value) => value === null),
  );
  assert.deepEqual(entityMatch(observed.items[4], [covered, missing], false), {
    hits: ["covered"],
    possible: [],
  });
});

test("stage line ranges match public LF, CRLF, UTF-8 and empty-range boundaries", () => {
  // [description, start line, end line, start byte, end byte, end column, public last line]
  const cases = [
    ["LF", 1, 2, 0, 2, 0, 1],
    ["CRLF", 1, 2, 0, 3, 0, 1],
    ["no newline", 1, 1, 0, 1, 1, 1],
    ["nonzero last column", 1, 2, 0, 3, 1, 2],
    ["empty file", 1, 1, 0, 0, 0, 1],
    ["empty range at EOF", 2, 2, 2, 2, 0, 2],
    ["covered blank line", 1, 3, 0, 3, 0, 2],
    ["UTF-8 followed by LF", 1, 2, 0, 4, 0, 1],
  ];
  for (const [name, first, last, start, end, column, publicLast] of cases) {
    const observed = fixture();
    observed.useBytes = false;
    observed.pair = { file_targets: pair.file_targets, entity_targets: [] };
    const range = text(first, last, start, end);
    range.text.end.byte_column = column;
    Object.assign(observed.capture.files[path].a, {
      range,
      loaded_range: range,
    });
    observed.items[4].range = {
      kind: "text",
      start_line: first,
      end_line: publicLast,
    };
    const scored = scoreGoldStages(observed);
    assert.equal(scored.state, "observed", name);
    assert.equal(scored.targets[0].coverage.actual_top10, true, name);
  }
});

test("configures session-wide paths without sending Gold labels or using files", () => {
  const extra = {
    file_targets: [],
    entity_targets: [{ id: "private-gold", path: "another.go" }],
  };
  assert.deepEqual(stageTraceEnvironment(true, [pair, extra, pair]), {
    ZG_TRACE_STAGES: "1",
    ZG_TRACE_PATHS: JSON.stringify(["another.go", path]),
  });
  assert.deepEqual(stageTraceEnvironment(false, [pair]), {
    ZG_TRACE_STAGES: "0",
    ZG_TRACE_PATHS: "[]",
  });
});

test("receives traces from the matching MCP response without carrying state across calls", () => {
  const { capture } = fixture();
  const response = { content: [], _meta: { zg_stage_trace: [capture] } };
  assert.deepEqual(readStageTrace(response), capture);
  assert.equal(readStageTrace({ content: [] }), null);
  assert.equal(readStageTrace({ _meta: {} }), null);
  assert.equal(readStageTrace({ ...response, isError: true }), null);
  assert.deepEqual(readStageTrace(response), capture);
});

test("distinguishes absent metadata from malformed metadata without failing search scoring", () => {
  const observed = fixture();
  const missing = scoreGoldStages({
    ...observed,
    capture: readStageTrace({ content: [] }),
  });
  assert.equal(missing.state, "unobserved");
  assert.equal(summarizeGoldStages([missing]).invalid_calls, 0);
  for (const [metadata, reason] of [
    [null, /must be an array/],
    [{}, /must be an array/],
    ["invalid", /must be an array/],
    [[], /expected one/],
    [[observed.capture, observed.capture], /expected one/],
    [[{ ...observed.capture, group_id: "Q2" }], /expected Q1/],
    [[null], /expected Q1/],
  ]) {
    const capture = readStageTrace({
      content: [],
      _meta: { zg_stage_trace: metadata },
    });
    assert.ok(capture instanceof Error);
    const scored = scoreGoldStages({ ...observed, capture });
    assert.equal(scored.state, "invalid");
    assert.match(scored.error, reason);
    assert.ok(
      scored.targets.every(({ coverage }) =>
        Object.values(coverage).every((value) => value === null),
      ),
    );
    const summary = summarizeGoldStages([scored]);
    assert.equal(summary.observed_calls, 0);
    assert.equal(summary.invalid_calls, 1);
  }
  assert.equal(scoreGoldStages(observed).state, "observed");
});

test("malformed capture payloads become invalid without retaining partial target scores", () => {
  for (const corrupt of [
    (capture) => {
      capture.version = 99;
    },
    (capture) => {
      delete capture.index_observed;
    },
    (capture) => {
      delete capture.files[path].b.recall;
    },
    // Unselected ranges are read only while scoring, after capture validation.
    (capture) => {
      capture.files[path].b.range = { text: { start: {} } };
    },
  ]) {
    const observed = fixture();
    corrupt(observed.capture);
    const capture = readStageTrace({
      _meta: { zg_stage_trace: [observed.capture] },
    });
    const scored = scoreGoldStages({ ...observed, capture });
    assert.equal(scored.state, "invalid");
    assert.ok(scored.error);
    assert.ok(
      scored.targets.every(({ coverage }) =>
        Object.values(coverage).every((value) => value === null),
      ),
    );
    assert.ok(
      scored.targets.every(({ ranks }) =>
        Object.values(ranks).every((value) => value === "unobserved"),
      ),
    );
    assert.equal(summarizeGoldStages([scored]).invalid_calls, 1);
  }
  assert.equal(scoreGoldStages(fixture()).state, "observed");
});

test("distinguishes an unobserved Gold path from an observed path missing in the index", () => {
  const evidence = fixture();
  evidence.capture.watched_paths = [];
  assert.equal(scoreGoldStages(evidence).state, "invalid");
  assert.equal(scoreGoldStages(evidence).targets[0].coverage.index, null);
  evidence.capture.watched_paths = [path];
  evidence.capture.files = {};
  evidence.items = evidence.items.map((item) => ({
    ...item,
    path: "other.go",
  }));
  assert.equal(scoreGoldStages(evidence).state, "observed");
  assert.equal(scoreGoldStages(evidence).targets[0].coverage.index, false);
});

test("scores a question within the session's larger observation scope", () => {
  const evidence = fixture();
  evidence.capture.watched_paths.push("another.go");
  evidence.capture.files["another.go"] = { unrelated: candidate(null, {}) };
  assert.equal(scoreGoldStages(evidence).state, "observed");
  assert.equal(scoreGoldStages(evidence).targets[1].coverage.selected, true);
});
