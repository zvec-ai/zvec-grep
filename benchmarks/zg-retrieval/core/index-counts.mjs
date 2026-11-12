import assert from "node:assert/strict";

function count(value, name) {
  assert.ok(
    value === null || (Number.isSafeInteger(value) && value >= 0),
    `invalid indexed ${name} count`,
  );
  return value;
}

// One entry per independently built index, never per query, mode or repetition.
export function summarizeIndexCounts(corpora) {
  assert.ok(Array.isArray(corpora), "invalid indexed corpora");
  const ids = new Set();
  const records = corpora.map((entry) => {
    assert.ok(
      typeof entry?.id === "string" &&
        entry.id.length > 0 &&
        entry.id.length <= 512 &&
        !/[\r\n\x00]/.test(entry.id),
      "invalid indexed corpus id",
    );
    assert.ok(!ids.has(entry.id), "duplicate indexed corpus");
    ids.add(entry.id);
    return {
      id: entry.id,
      entities: count(entry.entities, "Entity"),
      fragments: count(entry.fragments, "Fragment"),
    };
  });
  const total = (key) => {
    if (!records.length || records.some((record) => record[key] === null))
      return null;
    return count(
      records.reduce((sum, record) => sum + record[key], 0),
      key,
    );
  };
  return {
    entities: total("entities"),
    fragments: total("fragments"),
    corpora: records,
  };
}

export function validateIndexCounts(value) {
  assert.ok(value && typeof value === "object", "missing index counts");
  const result = summarizeIndexCounts(value.corpora);
  assert.equal(
    value.entities,
    result.entities,
    "Entity total differs from corpora",
  );
  assert.equal(
    value.fragments,
    result.fragments,
    "Fragment total differs from corpora",
  );
  return result;
}

export function indexCountsMarkdown(value) {
  const counts = value ? validateIndexCounts(value) : null;
  const number = (value) =>
    value == null ? "N/A" : value.toLocaleString("en-US");
  return `Indexed corpus: **${number(counts?.entities)} Entities · ${number(counts?.fragments)} Fragments**. Totals count each separately built index once; modes and repetitions do not multiply them. N/A means the count was not recorded.`;
}
