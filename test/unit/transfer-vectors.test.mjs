import assert from "node:assert/strict";
import test from "node:test";
import { compareStoredVectors } from "../../dist/engine/migrate/index.js";

for (const source of [1, -1]) {
  test(`cosine comparison accepts two float32 steps from ${source} and rejects three`, () => {
    const buffer = new ArrayBuffer(4);
    const floats = new Float32Array(buffer);
    const bits = new Uint32Array(buffer);
    floats[0] = source;
    const original = bits[0];
    for (const direction of [-1, 1]) {
      for (const steps of [2, 3]) {
        bits[0] = original + direction * steps;
        const result = compareStoredVectors([source], [floats[0]], {
          dimension: 1,
          metric: "cosine",
        });
        assert.deepEqual(result, { exact: false, preserved: steps === 2 });
      }
    }
  });
}

test("cosine comparison permits only two float32 steps and reports exactness separately", () => {
  const source = [8, -8, 0.125, -0.125];
  const next = source.map((v) => Math.fround(v * (1 + 2 ** -23)));
  assert.deepEqual(
    compareStoredVectors(source, source, { dimension: 4, metric: "cosine" }),
    { exact: true, preserved: true },
  );
  assert.deepEqual(
    compareStoredVectors(source, next, { dimension: 4, metric: "cosine" }),
    { exact: false, preserved: true },
  );
  for (const metric of ["dot", "euclidean"]) {
    assert.equal(
      compareStoredVectors(source, next, { dimension: 4, metric }).preserved,
      false,
    );
  }
  for (const invalid of [
    [8.01, -8, 0.125, -0.125],
    [8, 8, 0.125, -0.125],
    [Infinity, -8, 0.125, -0.125],
    [NaN, -8, 0.125, -0.125],
    [8],
    ["8", -8, 0.125, -0.125],
  ]) {
    assert.equal(
      compareStoredVectors(source, invalid, { dimension: 4, metric: "cosine" })
        .preserved,
      false,
    );
  }
  assert.equal(
    compareStoredVectors([1e-10], [0], { dimension: 1, metric: "cosine" })
      .preserved,
    false,
  );
});
