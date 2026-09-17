import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { posix } from "node:path";

export async function loadDataset(path, defaultCases) {
  if (!path)
    return {
      cases: defaultCases,
      layout: "src",
      label: "local src snapshot only",
    };
  const dataset = JSON.parse(await readFile(path, "utf8"));
  assert.ok(
    dataset && typeof dataset === "object",
    "Expected an evaluation dataset object",
  );
  assert.ok(
    ["src", "root"].includes(dataset.layout),
    "Dataset layout must be src or root",
  );
  assert.ok(
    typeof dataset.label === "string" && dataset.label.trim(),
    "Dataset label is required",
  );
  assert.ok(
    Array.isArray(dataset.cases) && dataset.cases.length > 0,
    "Dataset cases must be nonempty",
  );
  for (const item of dataset.cases) {
    assert.ok(item && typeof item === "object", "Each case must be an object");
    assert.ok(
      typeof item.query === "string" && item.query.trim(),
      "Each case needs a query",
    );
    assert.ok(
      [
        "symbol",
        "path",
        "phrase",
        "semantic-en",
        "semantic-zh",
        "mixed",
        "negative-symbol",
        "negative-path",
      ].includes(item.kind),
      "Unsupported case kind",
    );
    if (item.file !== undefined) {
      assert.ok(
        typeof item.file === "string" &&
          item.file.length > 0 &&
          !item.file.includes("\\") &&
          !/^[A-Za-z]:/.test(item.file) &&
          !posix.isAbsolute(item.file) &&
          posix.normalize(item.file) === item.file &&
          item.file !== ".." &&
          !item.file.startsWith("../") &&
          item.file !== ".",
        "Case file must stay inside the corpus",
      );
    }
    if (item.needle !== undefined) {
      assert.ok(
        item.file && typeof item.needle === "string" && item.needle.length > 0,
        "A source needle requires a file",
      );
    }
    if (item.cold !== undefined) assert.equal(typeof item.cold, "boolean");
    if (item.kind.startsWith("negative-")) {
      assert.ok(
        item.file === undefined && item.needle === undefined,
        "Negative cases must expect no source location",
      );
    } else {
      assert.ok(
        item.file && (item.kind === "path" || item.needle),
        "Positive cases require a grounded source location",
      );
    }
  }
  return dataset;
}
