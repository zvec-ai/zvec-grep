import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { loadDataset } from "../run.mjs";
import { fileHash, readJson, writeJson } from "../../zg-retrieval/core/io.mjs";

async function fixture(t) {
  const directory = await mkdtemp(join(tmpdir(), "browsecomp-dataset-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const lockPath = new URL("../source.lock.json", import.meta.url);
  const selectionPath = new URL("../suites/ci-selection.json", import.meta.url);
  const lock = await readJson(lockPath);
  const selection = await readJson(selectionPath);
  const manifestPath = join(directory, "corpus-manifest.jsonl");
  // loadDataset checks manifest identity; corpus coverage is checked by the runner.
  await writeFile(manifestPath, '{"path":"fixture.md","bytes":0}\n');
  const dataset = {
    schema_version: 1,
    quality_target: "evidence",
    source_lock_sha256: await fileHash(lockPath),
    selection_sha256: await fileHash(selectionPath),
    corpus: {
      count: lock.corpus.count,
      manifest_sha256: await fileHash(manifestPath),
    },
    tasks: selection.cases.map((row) => ({
      id: row.query_id,
      query: `Fixture query ${row.query_id}`,
      gold_docids: row.gold_docids,
      evidence_docids: row.evidence_docids,
      targets: row.evidence_docids.map((id) => ({ path: `${id}.md` })),
    })),
  };
  const save = () => writeJson(join(directory, "dataset.json"), dataset);
  await save();
  return { directory, dataset, lock, save };
}

test("the frozen selection loads with Evidence targets", async (t) => {
  const { directory, dataset, lock } = await fixture(t);
  const loaded = await loadDataset(directory);
  assert.deepEqual(loaded, { dataset, lock });
  const task = loaded.dataset.tasks.find(
    (row) => row.evidence_docids.length > row.gold_docids.length,
  );
  assert.ok(task, "the fixture must distinguish Evidence from Gold");
  const bridge = task.evidence_docids.find(
    (id) => !task.gold_docids.includes(id),
  );
  assert.ok(task.targets.some(({ path }) => path === `${bridge}.md`));
});

test("an Evidence marker cannot relabel Gold targets", async (t) => {
  const { directory, dataset, save } = await fixture(t);
  for (const task of dataset.tasks)
    task.targets = task.gold_docids.map((id) => ({ path: `${id}.md` }));
  await save();
  await assert.rejects(loadDataset(directory), { code: "ERR_ASSERTION" });
});

test("missing or incorrect scoring labels are rejected", async (t) => {
  for (const target of [undefined, "gold"]) {
    await t.test(target ?? "missing", async (t) => {
      const { directory, dataset, save } = await fixture(t);
      if (target === undefined) delete dataset.quality_target;
      else dataset.quality_target = target;
      await save();
      await assert.rejects(loadDataset(directory), { code: "ERR_ASSERTION" });
    });
  }
});

test("Evidence IDs must match the frozen annotations even with matching targets", async (t) => {
  const { directory, dataset, save } = await fixture(t);
  const task = dataset.tasks[0];
  task.evidence_docids = ["999999"];
  task.targets = [{ path: "999999.md" }];
  await save();
  await assert.rejects(loadDataset(directory), { code: "ERR_ASSERTION" });
});
