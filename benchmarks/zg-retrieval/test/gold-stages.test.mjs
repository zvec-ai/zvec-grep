import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { goldStageSession, readGoldStages } from "../core/gold-stage-capture.mjs";
import { indexedSourceOffsets } from "../expansion/gold.mjs";
import { scoreGoldStages, summarizeGoldStages } from "../metrics/gold-stages.mjs";
import { goldStagesMarkdown } from "../reports/gold-stages.mjs";

const path = "code.go";
const pair = {
  file_targets: [{ id: "file", path }],
  entity_targets: [
    { id: "first", path, start_line: 1, end_line: 2, start_byte: 1, end_byte: 22 },
    { id: "second", path, start_line: 9, end_line: 9, start_byte: 90, end_byte: 95 },
    { id: "third", path, start_line: 15, end_line: 16, start_byte: 141, end_byte: 166 },
  ],
};
const text = (first, last, start, end) => ({ text: {
  start: { line: first, byte_offset: start },
  end: { line: last, byte_offset: end },
} });

function candidate(range, stages) {
  return {
    range, searchable: true, recall: {}, merged: true, rrf_rank: null,
    loaded_rrf_rank: null, loaded_range: null, static_rank: null, final_rank: null,
    ...stages,
  };
}

function fixture() {
  const first = text(1, 2, 1, 22);
  const second = text(9, 9, 89, 98);
  const capture = {
    version: 1, id: 1, limit: 10, index_observed: true, index_read_ms: 2,
    routes: { fts: {}, vector: {} },
    sizes: { merged: 12, fused: 12, loaded: 11, selected: 10 },
    files: { [path]: {
      a: candidate(first, {
        recall: { fts: 200, vector: 5 }, rrf_rank: 2, loaded_rrf_rank: 2,
        loaded_range: first, static_rank: 11, final_rank: 5,
      }),
      b: candidate(second, {
        recall: { fts: 92 }, rrf_rank: 11, loaded_rrf_rank: 11,
        loaded_range: second, static_rank: 1,
      }),
      c: candidate(text(15, 16, 140, 167), { recall: { vector: 16 }, rrf_rank: 12 }),
    } },
  };
  const items = Array.from({ length: 10 }, (_, i) => ({ rank: i + 1, path: `other-${i}.go` }));
  items[4] = { rank: 5, path, range: { kind: "text", start_line: 1, end_line: 2 }, source_bytes: { start_byte: 1, end_byte: 22 } };
  return { capture, pair, items, taskId: "task", mode: "hybrid", repetition: 1, qualification: "diagnostic", useBytes: true };
}

test("measures full pools, route ranks, and top ten with and without same-file decay", () => {
  const scored = scoreGoldStages(fixture());
  assert.equal(scored.state, "observed");
  const [file, first, second, third] = scored.targets;
  assert.equal(file.coverage.selected, true);
  assert.deepEqual(first.ranks, { fts: 200, vector: 5, fused: 2, loaded: 2, selected: 5, static: 11 });
  assert.equal(first.coverage.static_top10, false);
  assert.equal(second.coverage.index, true);
  assert.equal(second.coverage.vector, false);
  assert.equal(second.coverage.static_top10, true);
  assert.equal(second.ranks.selected, "miss");
  assert.equal(third.coverage.fused, true);
  assert.equal(third.coverage.loaded, false);
  const entity = summarizeGoldStages([scored]).groups.find(({ level }) => level === "entity");
  assert.equal(entity.targets, 3);
  assert.deepEqual(entity.stages.loaded, { covered: 2, unknown: 0, lost: 1, loss_unknown: 0 });
  assert.equal(entity.stages.static_top10.lost, 1);
  assert.equal(entity.stages.actual_top10.lost, 1);
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
    entity_targets: [{
      id: "scifact-document",
      path,
      evidence_sets: [
        { id: "both", spans: [first, second] },
        { id: "alternative", spans: [third] },
      ],
    }],
  };
  const { a, c } = observed.capture.files[path];
  a.static_rank = 2;
  const score = () => scoreGoldStages(observed).targets[1];
  const complete = score();
  assert.equal(complete.ranks.fts, 200);
  assert.equal(complete.ranks.vector, 16);
  assert.equal(complete.ranks.fused, 11);
  assert.equal(complete.coverage.static_top10, true);
  assert.equal(complete.coverage.actual_top10, false);

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
  assert.deepEqual(scoreGoldStages(observed).targets.map(({ level }) => level), ["file"]);
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
    "| Same-file decay off | N/A | — | 2/3 |",
    "| Same-file decay on (actual) | N/A | — | 1/3 |",
  ]);
});

test("maps indexed offsets to frozen source bytes for a verified UTF-8 BOM", () => {
  const evidence = fixture();
  evidence.pair = { ...pair, entity_targets: pair.entity_targets.map((target) => ({
    ...target, start_byte: target.start_byte + 3, end_byte: target.end_byte + 3,
  })) };
  evidence.items[4].source_bytes = { start_byte: 4, end_byte: 25 };
  evidence.sourceOffsets = indexedSourceOffsets(new Map([
    [path, { bytes: Buffer.from("\uFEFFpackage example") }],
    ["plain.go", { bytes: Buffer.from("package example") }],
  ]));
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

test("binds capture bytes to the call and disables capture after a transport failure", async () => {
  const root = await mkdtemp(join(tmpdir(), "zg-gold-stages-"));
  try {
    const session = await goldStageSession(root, true);
    const id = await session.before("original question", "hybrid", pair);
    const filename = join(session.env.ZG_RETRIEVAL_GOLD_STAGE_DIR, `capture-${id}.json`);
    await writeFile(filename, JSON.stringify({ id }));
    const reference = await session.after(id, false);
    assert.deepEqual(await readGoldStages(root, reference), { id });
    await writeFile(filename, JSON.stringify({ id, changed: true }));
    assert.equal(await readGoldStages(root, reference), null);
    const next = await session.before("original question", "hybrid", pair);
    assert.equal(await session.after(next, true), null);
    assert.equal(await session.before("original question", "hybrid", pair), null);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
