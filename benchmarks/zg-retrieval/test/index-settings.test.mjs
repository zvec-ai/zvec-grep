import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  summarizeIndexSettings,
  validateIndexSettings,
  validateInputBudget,
} from "../core/index-settings.mjs";
import { snapshotIndex } from "../engines/zg/snapshot.mjs";

const budget = {
  max_input_tokens: 1024,
  unit: "utf16_code_units",
  rounding: "floor",
  overlap_percent: 15,
  normal: { chars_per_100_tokens: 185, max_length: 1894, overlap: 284 },
  dense: { chars_per_100_tokens: 100, max_length: 1024, overlap: 153 },
};
const entry = (id, inputBudget = budget) => ({
  id,
  model: "local/test-model",
  input_budget: inputBudget,
});

test("index settings retain per-index evidence and withhold incomplete common settings", () => {
  const corpora = [entry("a"), entry("b")];
  const summary = summarizeIndexSettings(corpora);
  assert.deepEqual(summary, {
    model: "local/test-model",
    input_budget: budget,
    corpora,
  });
  assert.deepEqual(validateIndexSettings(summary), summary);
  const unknownBudget = summarizeIndexSettings([entry("a"), entry("b", null)]);
  assert.equal(unknownBudget.model, "local/test-model");
  assert.equal(unknownBudget.input_budget, null);
  assert.deepEqual(unknownBudget.corpora[0].input_budget, budget);
  const unknownIndex = summarizeIndexSettings([
    entry("a"),
    { id: "b", model: null, input_budget: null },
  ]);
  assert.equal(unknownIndex.model, null);
  assert.equal(unknownIndex.input_budget, null);
  assert.deepEqual(summarizeIndexSettings([]), {
    model: null,
    input_budget: null,
    corpora: [],
  });
  corpora[0].input_budget = null;
  assert.deepEqual(summary.corpora[0].input_budget, budget);
});

test("conflicting or untrusted index settings are rejected, including among unknown corpora", () => {
  for (const entries of [
    [entry("a"), entry("a")],
    [entry("a"), { ...entry("b"), model: "local/another-model" }],
    [
      entry("a"),
      entry("b", null),
      entry("c", { ...budget, max_input_tokens: 2048 }),
    ],
    [{ ...entry("a"), model: "local/model\n| injected" }],
    [{ ...entry("a"), model: null }],
  ])
    assert.throws(
      () => summarizeIndexSettings(entries),
      /duplicate|conflicting|invalid|no model/,
    );
  const summary = summarizeIndexSettings([entry("a")]);
  assert.throws(
    () => validateIndexSettings({ ...summary, model: "local/another-model" }),
    /differs from corpora/,
  );
  assert.throws(
    () => validateIndexSettings({ ...summary, input_budget: null }),
    /differs from corpora/,
  );
});

test("input budgets validate reported structure without assuming a model or fixed ratios", () => {
  assert.equal(validateInputBudget(null), null);
  const otherVersion = {
    ...budget,
    max_input_tokens: 512,
    overlap_percent: 10,
    normal: { chars_per_100_tokens: 200, max_length: 1024, overlap: 102 },
    dense: { chars_per_100_tokens: 110, max_length: 563, overlap: 56 },
  };
  assert.deepEqual(validateInputBudget(otherVersion), otherVersion);
  for (const change of [
    { max_input_tokens: 0 },
    { max_input_tokens: "1024" },
    { max_input_tokens: Number.MAX_SAFE_INTEGER + 1 },
    { max_input_tokens: Infinity },
    { unit: "bytes" },
    { rounding: "ceil" },
    { overlap_percent: -1 },
    { overlap_percent: 100 },
    { normal: { ...budget.normal, chars_per_100_tokens: 0 } },
    { normal: { ...budget.normal, max_length: 0 } },
    { normal: { ...budget.normal, overlap: budget.normal.max_length } },
    { dense: { ...budget.dense, overlap: 0.5 } },
    { dense: null },
  ])
    assert.throws(() => validateInputBudget({ ...budget, ...change }));
});

test("one explicit debug status call records budgets and detects settings drift", async (t) => {
  const output = await mkdtemp(join(tmpdir(), "zg-index-settings-"));
  t.after(() => rm(output, { recursive: true, force: true }));
  const snapshots = [];
  let calls = 0;
  for (const [index, inputBudget] of [
    undefined,
    budget,
    { ...budget, overlap_percent: 10 },
  ].entries()) {
    const directory = join(output, String(index));
    snapshots.push(
      await snapshotIndex({
        cli: "zg",
        root: "/corpus",
        output: directory,
        runStatus: async (_cli, args) => {
          calls += 1;
          assert.deepEqual(args, [
            "--status",
            "/corpus",
            "--mode",
            "direct",
            "--check-ready",
            "--debug",
          ]);
          return {
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
                  embedding: {
                    provider: "local",
                    model: "test-model",
                    input_budget: inputBudget,
                  },
                },
                status: {
                  files_scanned: 1,
                  files_indexed: 1,
                  files_pending: 0,
                  files_failed: 0,
                  files_added: 0,
                  files_modified: 0,
                  files_deleted: 0,
                  files_unchanged: 1,
                  entities_indexed: 3,
                  fragments_indexed: 9,
                  indexed_size_bytes: 100,
                },
              },
            })}\n`,
          };
        },
      }),
    );
    const status = JSON.parse(
      await readFile(join(directory, "status.json"), "utf8"),
    );
    assert.deepEqual(status.input_budget, inputBudget ?? null);
    assert.deepEqual(snapshots[index].index_settings, {
      model: "local/test-model",
      input_budget: inputBudget ?? null,
    });
    assert.equal(snapshots[index].schema_version, 3);
  }
  assert.equal(calls, 3);
  assert.equal(
    new Set(snapshots.map((snapshot) => snapshot.logical_content_sha256)).size,
    3,
  );
});
