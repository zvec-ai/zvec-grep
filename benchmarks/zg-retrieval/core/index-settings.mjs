import assert from "node:assert/strict";

function positiveInteger(value, name) {
  assert.ok(
    Number.isSafeInteger(value) && value > 0,
    `invalid input budget ${name}`,
  );
  return value;
}

export function validateInputBudget(value) {
  if (value === null) return null;
  assert.ok(value && typeof value === "object", "invalid input budget");
  assert.equal(value.unit, "utf16_code_units", "invalid input budget unit");
  assert.equal(value.rounding, "floor", "invalid input budget rounding");
  assert.ok(
    Number.isSafeInteger(value.overlap_percent) &&
      value.overlap_percent >= 0 &&
      value.overlap_percent < 100,
    "invalid input budget overlap percent",
  );
  const profile = (name) => {
    const entry = value[name];
    assert.ok(entry && typeof entry === "object", `missing ${name} budget`);
    const maxLength = positiveInteger(entry.max_length, `${name} max length`);
    assert.ok(
      Number.isSafeInteger(entry.overlap) &&
        entry.overlap >= 0 &&
        entry.overlap < maxLength,
      `invalid input budget ${name} overlap`,
    );
    return {
      chars_per_100_tokens: positiveInteger(
        entry.chars_per_100_tokens,
        `${name} character ratio`,
      ),
      max_length: maxLength,
      overlap: entry.overlap,
    };
  };
  return {
    max_input_tokens: positiveInteger(value.max_input_tokens, "token limit"),
    unit: value.unit,
    rounding: value.rounding,
    overlap_percent: value.overlap_percent,
    normal: profile("normal"),
    dense: profile("dense"),
  };
}

function modelName(value) {
  assert.ok(
    value === null ||
      (typeof value === "string" &&
        value.length <= 512 &&
        /^[A-Za-z0-9][A-Za-z0-9._:/@+-]*$/.test(value)),
    "invalid indexed model",
  );
  return value;
}

// Each entry describes one independently built index, never a query or mode.
export function summarizeIndexSettings(corpora) {
  assert.ok(Array.isArray(corpora), "invalid index settings corpora");
  const ids = new Set();
  const records = corpora.map((entry) => {
    assert.ok(
      typeof entry?.id === "string" &&
        entry.id.length > 0 &&
        entry.id.length <= 512 &&
        !/[\x00-\x1f\x7f]/.test(entry.id),
      "invalid index settings corpus id",
    );
    assert.ok(!ids.has(entry.id), "duplicate index settings corpus");
    ids.add(entry.id);
    const model = modelName(entry.model);
    const inputBudget = validateInputBudget(entry.input_budget);
    assert.ok(
      model !== null || inputBudget === null,
      "input budget has no model",
    );
    return { id: entry.id, model, input_budget: inputBudget };
  });
  const common = (key) => {
    const known = records.filter((record) => record[key] !== null);
    for (const record of known.slice(1))
      assert.deepEqual(record[key], known[0][key], `conflicting index ${key}`);
    return records.length && known.length === records.length
      ? known[0][key]
      : null;
  };
  return {
    model: common("model"),
    input_budget: common("input_budget"),
    corpora: records,
  };
}

export function validateIndexSettings(value) {
  assert.ok(value && typeof value === "object", "missing index settings");
  const result = summarizeIndexSettings(value.corpora);
  assert.equal(value.model, result.model, "index model differs from corpora");
  assert.deepEqual(
    value.input_budget,
    result.input_budget,
    "input budget differs from corpora",
  );
  return result;
}
