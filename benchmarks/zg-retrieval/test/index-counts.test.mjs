import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  summarizeIndexCounts,
  validateIndexCounts,
} from "../core/index-counts.mjs";
import { snapshotIndex } from "../engines/zg/snapshot.mjs";
import { loadPilot, pilotCorpusIds } from "../expansion/datasets.mjs";

test("index totals count corpora once and never turn missing counts into partial totals", () => {
  const corpora = [
    { id: "revision-a", entities: 10, fragments: 25 },
    { id: "revision-b", entities: 12, fragments: 30 },
  ];
  assert.deepEqual(summarizeIndexCounts(corpora), {
    entities: 22,
    fragments: 55,
    corpora,
  });
  assert.throws(
    () => summarizeIndexCounts([...corpora, corpora[0]]),
    /duplicate/,
  );
  assert.equal(
    summarizeIndexCounts([{ ...corpora[0], fragments: null }, corpora[1]])
      .fragments,
    null,
  );
  assert.equal(summarizeIndexCounts([]).entities, null);
  assert.equal(
    summarizeIndexCounts([{ id: "empty", entities: 0, fragments: 0 }])
      .fragments,
    0,
  );
  assert.throws(
    () => validateIndexCounts({ entities: 23, fragments: 55, corpora }),
    /total/,
  );
  for (const bad of [-1, 1.1, "25", Number.MAX_SAFE_INTEGER + 1])
    assert.throws(
      () => summarizeIndexCounts([{ ...corpora[0], fragments: bad }]),
      /invalid/,
    );
  assert.throws(
    () =>
      summarizeIndexCounts([
        { id: "a", entities: Number.MAX_SAFE_INTEGER, fragments: null },
        { id: "b", entities: 1, fragments: null },
      ]),
    /invalid/,
  );
});

test("pilot inventory describes independent corpus builds, not questions or modes", async () => {
  const beir = pilotCorpusIds(await loadPilot("beir"));
  assert.equal(beir.length, 6);
  assert.ok(
    ["arguana-1", "arguana-2", "arguana-3"].every((id) => beir.includes(id)),
  );
  assert.deepEqual(pilotCorpusIds(await loadPilot("duretrieval")), [
    "duretrieval",
  ]);
  assert.deepEqual(
    pilotCorpusIds(await loadPilot("quarry")),
    Array.from({ length: 20 }, (_, i) => `task-${i + 1}`),
  );
});

test("status snapshots keep Entity and Fragment totals distinct and detect Fragment drift", async (t) => {
  const output = await mkdtemp(join(tmpdir(), "zg-counts-"));
  t.after(() => rm(output, { recursive: true, force: true }));
  const snapshots = [];
  for (const [index, fragments] of [undefined, 9, 10].entries()) {
    const status = {
      files_scanned: 1,
      files_indexed: 1,
      files_pending: 0,
      files_failed: 0,
      files_added: 0,
      files_modified: 0,
      files_deleted: 0,
      files_unchanged: 1,
      entities_indexed: 3,
      indexed_size_bytes: 100,
      fragments_indexed: fragments,
    };
    snapshots.push(
      await snapshotIndex({
        cli: "zg",
        root: "/corpus",
        output: join(output, String(index)),
        runStatus: async () => ({
          stdout: "",
          code: 0,
          signal: null,
          timed_out: false,
          stderr: `Status diagnostics: ${JSON.stringify({
            state: "ready",
            info: {
              indexed: true,
              index_policy: "enabled",
              compatibility: { status: "compatible" },
              root: "/corpus",
              index_path: "/corpus/.zvec-grep",
              workspace_index: {
                embedding: { provider: "local", model: "test" },
              },
              status,
            },
          })}\n`,
        }),
      }),
    );
  }
  assert.deepEqual(
    snapshots.map(({ entities, fragments }) => [entities, fragments]),
    [
      [3, null],
      [3, 9],
      [3, 10],
    ],
  );
  assert.notEqual(
    snapshots[1].logical_content_sha256,
    snapshots[2].logical_content_sha256,
  );
});
