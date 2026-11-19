import assert from "node:assert/strict";
import test from "node:test";
import { summarizeIndexSettings } from "../core/index-settings.mjs";
import {
  indexSettingsMarkdown,
  indexSettingsComparisonMarkdown,
} from "../reports/index-settings.mjs";

const budget = {
  max_input_tokens: 1024,
  unit: "utf16_code_units",
  rounding: "floor",
  overlap_percent: 15,
  normal: { chars_per_100_tokens: 185, max_length: 1894, overlap: 284 },
  dense: { chars_per_100_tokens: 100, max_length: 1024, overlap: 153 },
};
const model = "local/potion-code-16m-v2";
const settings = (input_budget) =>
  summarizeIndexSettings([{ id: "corpus", model, input_budget }]);

test("reports display collected budgets, units and unknown baseline without guessing", () => {
  const candidate = settings(budget);
  const markdown = indexSettingsComparisonMarkdown(
    { baseline: settings(null), candidate },
    model,
  );
  assert.match(markdown, /ZG input limit: \*\*N\/A → 1,024 tokens\*\*/);
  assert.match(markdown, /UTF-16 units, length \/ overlap/);
  assert.match(markdown, /normal \*\*N\/A → 1,894 \/ 284\*\*/);
  assert.match(markdown, /dense \*\*N\/A → 1,024 \/ 153\*\*/);
  assert.match(markdown, /N\/A → 1\.85/);
  assert.match(markdown, /N\/A → 15%/);
  assert.match(markdown, /N\/A → floor/);
  const identical = indexSettingsComparisonMarkdown(
    { baseline: candidate, candidate },
    model,
  );
  assert.doesNotMatch(identical, /→/);
  assert.equal(identical, indexSettingsMarkdown(candidate));
  assert.match(
    indexSettingsComparisonMarkdown(undefined, model),
    /ZG input limit: \*\*N\/A\*\*/,
  );
  assert.throws(
    () => indexSettingsMarkdown(candidate, "local/wrong"),
    /differs from report/,
  );
});

test("remote model reports retain the larger configured budget", () => {
  const remote = summarizeIndexSettings([
    {
      id: "corpus",
      model: "dashscope/qwen3.7-text-embedding",
      input_budget: {
        ...budget,
        max_input_tokens: 128000,
        normal: {
          chars_per_100_tokens: 185,
          max_length: 236800,
          overlap: 35520,
        },
        dense: {
          chars_per_100_tokens: 100,
          max_length: 128000,
          overlap: 19200,
        },
      },
    },
  ]);
  const markdown = indexSettingsMarkdown(remote);
  assert.match(markdown, /128,000 tokens/);
  assert.match(markdown, /236,800 \/ 35,520/);
  assert.match(markdown, /128,000 \/ 19,200/);
});
