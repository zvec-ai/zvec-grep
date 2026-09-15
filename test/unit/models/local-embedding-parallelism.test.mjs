import assert from "node:assert/strict";
import test from "node:test";
import {
  INDEX_EMBEDDING_CONCURRENCY_ENV,
  normalizeLocalEmbeddingConcurrency,
  resolveLocalEmbeddingParallelism,
  resolveIndexEmbeddingConcurrencyOverride,
} from "../../../dist/engine/models/local-embedding-parallelism.js";

const LEGACY_LLAMA_PARALLELISM_ENV = "ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM";

test("index concurrency prefers explicit values and leaves backend caps to the caller", (t) => {
  resetParallelismEnvironment(t);
  process.env[INDEX_EMBEDDING_CONCURRENCY_ENV] = "invalid";
  process.env[LEGACY_LLAMA_PARALLELISM_ENV] = "8";
  const warnings = [];
  t.mock.method(process.stderr, "write", (message) => {
    warnings.push(String(message));
    return true;
  });
  assert.equal(
    resolveIndexEmbeddingConcurrencyOverride({
      embeddingConcurrency: 1,
      legacyLlama: true,
    }),
    1,
  );
  assert.equal(
    resolveIndexEmbeddingConcurrencyOverride({ embeddingConcurrency: 99 }),
    99,
  );
  assert.deepEqual(warnings, []);
  for (const embeddingConcurrency of [
    0,
    -1,
    1.5,
    NaN,
    Infinity,
    Number.MAX_SAFE_INTEGER + 1,
  ]) {
    assert.throws(
      () => resolveIndexEmbeddingConcurrencyOverride({ embeddingConcurrency }),
      /positive integer/,
    );
  }
});

test("backend concurrency validates and caps explicit values without reading index or legacy environment", (t) => {
  resetParallelismEnvironment(t);
  process.env[INDEX_EMBEDDING_CONCURRENCY_ENV] = "8";
  process.env[LEGACY_LLAMA_PARALLELISM_ENV] = "8";
  assert.equal(normalizeLocalEmbeddingConcurrency(), undefined);
  assert.equal(normalizeLocalEmbeddingConcurrency(1), 1);
  assert.equal(normalizeLocalEmbeddingConcurrency(2), 2);
  assert.equal(normalizeLocalEmbeddingConcurrency(99), 8);
  for (const value of [
    0,
    -1,
    1.5,
    NaN,
    Infinity,
    Number.MAX_SAFE_INTEGER + 1,
  ]) {
    assert.throws(
      () => normalizeLocalEmbeddingConcurrency(value),
      /positive integer/,
    );
  }
});

function resetParallelismEnvironment(t) {
  for (const name of [
    INDEX_EMBEDDING_CONCURRENCY_ENV,
    LEGACY_LLAMA_PARALLELISM_ENV,
  ]) {
    const previous = process.env[name];
    delete process.env[name];
    t.after(() => {
      if (previous === undefined) {
        delete process.env[name];
      } else {
        process.env[name] = previous;
      }
    });
  }
}

test("index embedding parallelism prefers the shared override and makes legacy opt-in", (t) => {
  resetParallelismEnvironment(t);
  assert.equal(resolveIndexEmbeddingConcurrencyOverride(), undefined);

  process.env[LEGACY_LLAMA_PARALLELISM_ENV] = "3";
  assert.equal(resolveIndexEmbeddingConcurrencyOverride(), undefined);
  assert.equal(
    resolveIndexEmbeddingConcurrencyOverride({ legacyLlama: true }),
    3,
  );

  process.env[INDEX_EMBEDDING_CONCURRENCY_ENV] = " 5 ";
  assert.equal(resolveIndexEmbeddingConcurrencyOverride(), 5);
  assert.equal(
    resolveIndexEmbeddingConcurrencyOverride({ legacyLlama: true }),
    5,
  );

  process.env[INDEX_EMBEDDING_CONCURRENCY_ENV] = " \t ";
  assert.equal(resolveIndexEmbeddingConcurrencyOverride(), undefined);
  assert.equal(
    resolveIndexEmbeddingConcurrencyOverride({ legacyLlama: true }),
    3,
  );
});

test("index embedding parallelism accepts uncapped positive safe integers", (t) => {
  resetParallelismEnvironment(t);
  const warnings = [];
  t.mock.method(process.stderr, "write", (message) => {
    warnings.push(String(message));
    return true;
  });

  for (const value of [
    "0",
    "000",
    "-1",
    "1.5",
    "2workers",
    "1e2",
    "NaN",
    "9".repeat(400),
  ]) {
    process.env[INDEX_EMBEDDING_CONCURRENCY_ENV] = value;
    assert.equal(resolveIndexEmbeddingConcurrencyOverride(), undefined);
    assert.match(warnings.at(-1), /using automatic parallelism/);
    assert.ok(warnings.at(-1).includes(INDEX_EMBEDDING_CONCURRENCY_ENV));
  }
  assert.equal(warnings.length, 8);

  for (const value of ["8", "99", String(Number.MAX_SAFE_INTEGER)]) {
    process.env[INDEX_EMBEDDING_CONCURRENCY_ENV] = value;
    assert.equal(resolveIndexEmbeddingConcurrencyOverride(), Number(value));
  }
  process.env[INDEX_EMBEDDING_CONCURRENCY_ENV] = "1";
  assert.equal(resolveIndexEmbeddingConcurrencyOverride(), 1);
  process.env[INDEX_EMBEDDING_CONCURRENCY_ENV] = "03";
  assert.equal(resolveIndexEmbeddingConcurrencyOverride(), 3);
  assert.equal(warnings.length, 8);
});

test("an invalid shared override selects automatic mode instead of the legacy override", (t) => {
  resetParallelismEnvironment(t);
  const warnings = [];
  t.mock.method(process.stderr, "write", (message) => {
    warnings.push(String(message));
    return true;
  });
  process.env[INDEX_EMBEDDING_CONCURRENCY_ENV] = "invalid";
  process.env[LEGACY_LLAMA_PARALLELISM_ENV] = "3";
  assert.equal(
    resolveIndexEmbeddingConcurrencyOverride({ legacyLlama: true }),
    undefined,
  );
  assert.ok(warnings[0].includes(INDEX_EMBEDDING_CONCURRENCY_ENV));

  delete process.env[INDEX_EMBEDDING_CONCURRENCY_ENV];
  process.env[LEGACY_LLAMA_PARALLELISM_ENV] = "2.5";
  assert.equal(
    resolveIndexEmbeddingConcurrencyOverride({ legacyLlama: true }),
    undefined,
  );
  assert.ok(warnings[1].includes(LEGACY_LLAMA_PARALLELISM_ENV));
});

test("explicit local embedding parallelism bypasses GPU detection and CPU uses one worker", async () => {
  let reads = 0;
  const getVramState = async () => {
    reads++;
    return { free: 8 * 1024 * 1024 * 1024 };
  };
  for (const gpu of [false, true]) {
    assert.equal(
      await resolveLocalEmbeddingParallelism({
        override: 3,
        gpu,
        getVramState,
      }),
      3,
    );
  }
  assert.equal(
    await resolveLocalEmbeddingParallelism({ gpu: false, getVramState }),
    1,
  );
  assert.equal(await resolveLocalEmbeddingParallelism({ gpu: true }), 1);
  assert.equal(reads, 0);
});

test("automatic GPU parallelism uses a quarter of free VRAM at 150 MiB per worker", async () => {
  for (const [freeMb, expected] of [
    [0, 1],
    [599, 1],
    [600, 1],
    [1199, 1],
    [1200, 2],
    [1800, 3],
    [4799, 7],
    [4800, 8],
    [24 * 1024, 8],
  ]) {
    assert.equal(
      await resolveLocalEmbeddingParallelism({
        gpu: true,
        getVramState: async () => ({ free: freeMb * 1024 * 1024 }),
      }),
      expected,
      `free VRAM: ${freeMb} MiB`,
    );
  }
});

test("automatic GPU parallelism falls back to two when VRAM is unavailable or invalid", async () => {
  assert.equal(
    await resolveLocalEmbeddingParallelism({
      gpu: true,
      getVramState: async () => {
        throw new Error("VRAM unavailable");
      },
    }),
    2,
  );
  for (const free of [-1, NaN, Infinity, -Infinity, undefined, "1024"]) {
    assert.equal(
      await resolveLocalEmbeddingParallelism({
        gpu: true,
        getVramState: async () => ({ free }),
      }),
      2,
    );
  }
});
