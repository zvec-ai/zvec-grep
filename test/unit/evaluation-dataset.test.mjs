import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { loadDataset } from "../evaluation/dataset.mjs";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

test("external evaluation datasets preserve real repository paths and validate labels before querying", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-evaluation-cases-");
  const path = join(root, "cases.json");
  const cases = [
    {
      query: "loadConfig",
      kind: "symbol",
      file: "lib/config.js",
      needle: "function loadConfig",
      cold: true,
    },
  ];
  assert.deepEqual(await loadDataset(undefined, cases), {
    cases,
    layout: "src",
    label: "local src snapshot only",
  });
  const dataset = {
    cases,
    layout: "root",
    label: "public repository at pinned revision",
  };
  await writeFile(path, JSON.stringify(dataset));
  assert.deepEqual(await loadDataset(path), dataset);
  for (const file of [
    "/etc/passwd",
    "../outside",
    "lib/../../outside",
    "lib/../outside",
    "..",
    ".",
    "",
    "a\\b",
  ]) {
    await writeFile(
      path,
      JSON.stringify({ ...dataset, cases: [{ ...cases[0], file }] }),
    );
    await assert.rejects(loadDataset(path), /inside the corpus/);
  }
  for (const invalid of [
    { ...dataset, layout: ".." },
    { ...dataset, cases: [] },
    { ...dataset, cases: [{ query: "", kind: "symbol" }] },
    { ...dataset, cases: [{ query: "q", kind: "phrase", needle: "text" }] },
  ]) {
    await writeFile(path, JSON.stringify(invalid));
    await assert.rejects(loadDataset(path));
  }
});
