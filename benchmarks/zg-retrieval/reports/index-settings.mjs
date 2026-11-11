import assert from "node:assert/strict";
import { validateIndexSettings } from "../core/index-settings.mjs";

const number = (value) => value.toLocaleString("en-US");
const lengthAndOverlap = (value) =>
  `${number(value.max_length)} / ${number(value.overlap)}`;

function settings(value, expectedModel) {
  if (value == null) return null;
  const checked = validateIndexSettings(value);
  if (expectedModel && checked.model)
    assert.equal(
      checked.model,
      expectedModel,
      "indexed model differs from report",
    );
  return checked.input_budget;
}

function modelName(value) {
  if (!value) return "N/A";
  assert.match(
    value,
    /^[A-Za-z0-9][A-Za-z0-9._:/@+-]*$/,
    "invalid embedding model",
  );
  return `\`${value}\``;
}

function lines(model, cell) {
  return [
    `Embedding: ${modelName(model)} · ZG input limit: **${cell(
      (value) => value.max_input_tokens,
      (value) => `${number(value)} tokens`,
    )}**.`,
    `Fragment budget (UTF-16 units, length / overlap): normal **${cell((value) => value.normal, lengthAndOverlap)}**; dense **${cell((value) => value.dense, lengthAndOverlap)}**.`,
    `Budget rule: normal **${cell((value) => value.normal.chars_per_100_tokens / 100, String)}**, dense **${cell((value) => value.dense.chars_per_100_tokens / 100, String)}** UTF-16 units/token; overlap **${cell(
      (value) => value.overlap_percent,
      (value) => `${value}%`,
    )}**; rounding **${cell((value) => value.rounding, String)}**.`,
  ].join("  \n");
}

export function indexSettingsMarkdown(value, model = value?.model) {
  const budget = settings(value, model);
  return lines(model, (get, format) =>
    budget == null ? "N/A" : format(get(budget)),
  );
}

export function indexSettingsComparisonMarkdown(pair, model) {
  const before = settings(pair?.baseline, model);
  const after = settings(pair?.candidate, model);
  return lines(model, (get, format) => {
    const left = before == null ? "N/A" : format(get(before));
    const right = after == null ? "N/A" : format(get(after));
    return left === right ? left : `${left} → ${right}`;
  });
}
