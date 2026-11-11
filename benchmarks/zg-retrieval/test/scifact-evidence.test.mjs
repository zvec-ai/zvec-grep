import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { after, before, test } from "node:test";
import { sha256 } from "../core/io.mjs";
import {
  locateFullEntity,
  parseVisibleResponse,
} from "../engines/zg/parse.mjs";
import { loadPilot } from "../expansion/datasets.mjs";
import {
  summarizePilotEntity,
  validatePilotGold,
  verifyPilotGoldSources,
} from "../expansion/gold.mjs";

const fixture = JSON.parse(
  await readFile(new URL("./fixtures/scifact-evidence.json", import.meta.url)),
);
const gold = JSON.parse(
  await readFile(new URL("../expansion/gold/beir.json", import.meta.url)),
);
const lockBytes = await readFile(
  new URL("../expansion/data/beir20.json", import.meta.url),
);
const pilot = await loadPilot("beir");
const cases = new Map(fixture.cases.map((row) => [row.id, row]));

// Only the four evidence documents are needed here. The production manifest
// also retains qrels-only files; loadPilot above validates all original labels.
const sourceGold = {
  tasks: Object.fromEntries(
    fixture.cases.map(({ id, path }) => [
      id,
      {
        ...gold.tasks[id],
        files: gold.tasks[id].files.filter((file) => file.path === path),
      },
    ]),
  ),
};
let directory, group, files;
before(async () => {
  directory = await mkdtemp(join(tmpdir(), "zg-scifact-evidence-"));
  for (const row of fixture.cases) {
    const path = join(directory, row.path);
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, row.source_text);
  }
  group = {
    root: directory,
    tasks: fixture.cases.map(({ id }) => ({ id })),
  };
  files = await verifyPilotGoldSources(sourceGold, group);
});
after(async () => {
  if (directory) await rm(directory, { recursive: true, force: true });
});

function scoreSentences(id, sentences) {
  const row = cases.get(id);
  const response = {
    content: [
      {
        type: "text",
        text: `freshness: fresh\n${sentences
          .map(
            (sentence, index) =>
              `#${index + 1} matchedBy=fts ${row.path}:3-3\nsource:\n3\t${row.evidence_sentences[sentence]}`,
          )
          .join("\n\n")}\n`,
      },
    ],
  };
  const items = parseVisibleResponse(response).items.map((item) => {
    const source_bytes = locateFullEntity(item, files.get(item.path));
    assert.ok(source_bytes, "The full returned sentence must align uniquely");
    return { ...item, source_bytes };
  });
  return summarizePilotEntity(
    [1, 2, 3, 4, 5].map((repetition) => ({
      repetition,
      status: "success",
      items,
    })),
    gold.tasks[id],
  ).entity;
}

test("SciFact Gold preserves every original alternative and exact sentence span", () => {
  assert.equal(
    fixture.provenance.archive_sha256,
    gold.scifact_evidence.archive_sha256,
  );
  assert.equal(fixture.provenance.beir_source_lock_sha256, sha256(lockBytes));
  for (const row of fixture.cases) {
    assert.equal(sha256(row.source_text), row.source_sha256);
    assert.equal(
      pilot.lock.tasks.find((task) => task.id === row.id).query,
      row.query,
    );
    const [target] = gold.tasks[row.id].targets;
    assert.equal(gold.tasks[row.id].targets.length, 1);
    assert.equal(target.source_id, row.document_id);
    assert.deepEqual(
      target.evidence_sets.map((set) => set.scifact_sentences),
      row.original_evidence.map((set) => set.sentences),
    );
    const source = Buffer.from(row.source_text);
    for (const [index, set] of target.evidence_sets.entries()) {
      const sentences = row.original_evidence[index].sentences;
      assert.equal(set.spans.length, sentences.length);
      for (const [member, span] of set.spans.entries()) {
        assert.equal(
          source.subarray(span.start_byte, span.end_byte).toString("utf8"),
          row.evidence_sentences[sentences[member]],
        );
      }
    }
  }
});

test("SciFact verifies fixed source files and every evidence span checksum", async () => {
  assert.equal(files.size, 4);
  const changedFile = structuredClone(sourceGold);
  changedFile.tasks["scifact/436"].files[0].sha256 = "0".repeat(64);
  await assert.rejects(
    verifyPilotGoldSources(changedFile, group),
    /Changed Gold file:/,
  );
  const changedSpan = structuredClone(sourceGold);
  changedSpan.tasks["scifact/436"].targets[0].evidence_sets[1].spans[0].sha256 =
    "0".repeat(64);
  await assert.rejects(
    verifyPilotGoldSources(changedSpan, group),
    /Changed Gold span:/,
  );
});

test("SciFact 436 accepts its second alternative without double-counting the document", () => {
  for (const sentences of [[2], [1, 2], [2, 2, 1]]) {
    const score = scoreSentences("scifact/436", sentences);
    assert.equal(score.n_relevant, 1);
    assert.equal(score.recall_at_1, 1);
    assert.equal(score.recall_at_10, 1);
    assert.equal(score.rr_at_10, 1);
    assert.equal(score.ndcg_at_10, 1);
  }
});

test("SciFact 1274 requires both members and credits the rank that completes a set", () => {
  for (const sentences of [[4], [5], [4, 4]])
    assert.equal(scoreSentences("scifact/1274", sentences).recall_at_10, 0);
  const complete = scoreSentences("scifact/1274", [4, 5]);
  assert.equal(complete.recall_at_1, 0);
  assert.equal(complete.recall_at_5, 1);
  assert.equal(complete.rr_at_10, 1 / 2);
  assert.equal(complete.ndcg_at_10, 1 / Math.log2(3));
  const late = scoreSentences("scifact/1274", [4, 4, 4, 4, 4, 5]);
  assert.equal(late.recall_at_5, 0);
  assert.equal(late.recall_at_10, 1);
  assert.equal(late.rr_at_10, 1 / 6);
  assert.equal(late.ndcg_at_10, 1 / Math.log2(7));
  assert.equal(scoreSentences("scifact/1274", [7]).recall_at_1, 1);
});

test("SciFact 137 accepts noncontiguous sentences 7 and 9 without requiring sentence 8", () => {
  for (const sentences of [[7], [9]])
    assert.equal(scoreSentences("scifact/137", sentences).recall_at_10, 0);
  const complete = scoreSentences("scifact/137", [7, 9]);
  assert.equal(complete.recall_at_1, 0);
  assert.equal(complete.recall_at_5, 1);
  assert.equal(complete.rr_at_10, 1 / 2);
  assert.equal(scoreSentences("scifact/1163", [5]).recall_at_1, 1);
});

test("the loader rejects malformed alternatives and targets detached from their source", () => {
  const rejected = (mutate, message) => {
    const changed = structuredClone(gold);
    mutate(changed.tasks["scifact/436"].targets[0], changed);
    assert.throws(
      () => validatePilotGold(changed, pilot.lock, lockBytes),
      message,
    );
  };
  rejected((target) => {
    target.evidence_sets[1].id = target.evidence_sets[0].id;
  }, /duplicate evidence set ID/);
  rejected((target) => {
    target.evidence_sets[1].spans = target.evidence_sets[0].spans;
  }, /Duplicate evidence alternative/);
  rejected((target) => {
    target.evidence_sets[0].spans = [];
  }, /Empty evidence set/);
  rejected((target) => {
    target.evidence_sets = [];
  }, /Empty evidence alternatives/);
  rejected((target) => {
    target.source_id = "not-an-original-document";
  }, /does not match its original label/);
  rejected((_, changed) => {
    changed.schema_version = 1;
  }, /1 !== 2/);
});
