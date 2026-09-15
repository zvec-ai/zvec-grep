import assert from "node:assert/strict";
import test from "node:test";
import {
  LOCAL_EMBEDDING_CONCURRENCY_ENV,
  resolveLocalEmbeddingParallelism,
  resolveLocalEmbeddingParallelismOverride,
} from "../../../dist/engine/models/local-embedding-parallelism.js";

const LEGACY_LLAMA_PARALLELISM_ENV = "ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM";

test("explicit concurrency overrides both environment variables and shares the cap", (t) => {
  resetParallelismEnvironment(t);
  process.env[LOCAL_EMBEDDING_CONCURRENCY_ENV] = "invalid";
  process.env[LEGACY_LLAMA_PARALLELISM_ENV] = "8";
  const warnings = [];
  t.mock.method(process.stderr, "write", (message) => {
    warnings.push(String(message));
    return true;
  });
  assert.equal(
    resolveLocalEmbeddingParallelismOverride({
      embeddingConcurrency: 1,
      legacyLlama: true,
    }),
    1,
  );
  assert.equal(
    resolveLocalEmbeddingParallelismOverride({ embeddingConcurrency: 99 }),
    8,
  );
  assert.deepEqual(warnings, []);
  for (const embeddingConcurrency of [0, -1, 1.5, NaN, Infinity]) {
    assert.throws(
      () => resolveLocalEmbeddingParallelismOverride({ embeddingConcurrency }),
      /positive integer/,
    );
  }
});

function resetParallelismEnvironment(t) {
  for (const name of [
    LOCAL_EMBEDDING_CONCURRENCY_ENV,
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

test("local embedding parallelism prefers the shared override and makes legacy opt-in", (t) => {
  resetParallelismEnvironment(t);
  assert.equal(resolveLocalEmbeddingParallelismOverride(), undefined);

  process.env[LEGACY_LLAMA_PARALLELISM_ENV] = "3";
  assert.equal(resolveLocalEmbeddingParallelismOverride(), undefined);
  assert.equal(
    resolveLocalEmbeddingParallelismOverride({ legacyLlama: true }),
    3,
  );

  process.env[LOCAL_EMBEDDING_CONCURRENCY_ENV] = " 5 ";
  assert.equal(resolveLocalEmbeddingParallelismOverride(), 5);
  assert.equal(
    resolveLocalEmbeddingParallelismOverride({ legacyLlama: true }),
    5,
  );

  process.env[LOCAL_EMBEDDING_CONCURRENCY_ENV] = " \t ";
  assert.equal(resolveLocalEmbeddingParallelismOverride(), undefined);
  assert.equal(
    resolveLocalEmbeddingParallelismOverride({ legacyLlama: true }),
    3,
  );
});

test("local embedding parallelism validates positive integer overrides and caps them at eight", (t) => {
  resetParallelismEnvironment(t);
  const warnings = [];
  t.mock.method(process.stderr, "write", (message) => {
    warnings.push(String(message));
    return true;
  });

  for (const value of ["0", "000", "-1", "1.5", "2workers", "1e2", "NaN"]) {
    process.env[LOCAL_EMBEDDING_CONCURRENCY_ENV] = value;
    assert.equal(resolveLocalEmbeddingParallelismOverride(), undefined);
    assert.match(warnings.at(-1), /using automatic parallelism/);
    assert.ok(warnings.at(-1).includes(LOCAL_EMBEDDING_CONCURRENCY_ENV));
  }
  assert.equal(warnings.length, 7);

  for (const value of ["8", "99", "9".repeat(400)]) {
    process.env[LOCAL_EMBEDDING_CONCURRENCY_ENV] = value;
    assert.equal(resolveLocalEmbeddingParallelismOverride(), 8);
  }
  process.env[LOCAL_EMBEDDING_CONCURRENCY_ENV] = "1";
  assert.equal(resolveLocalEmbeddingParallelismOverride(), 1);
  process.env[LOCAL_EMBEDDING_CONCURRENCY_ENV] = "03";
  assert.equal(resolveLocalEmbeddingParallelismOverride(), 3);
  assert.equal(warnings.length, 7);
});

test("an invalid shared override selects automatic mode instead of the legacy override", (t) => {
  resetParallelismEnvironment(t);
  const warnings = [];
  t.mock.method(process.stderr, "write", (message) => {
    warnings.push(String(message));
    return true;
  });
  process.env[LOCAL_EMBEDDING_CONCURRENCY_ENV] = "invalid";
  process.env[LEGACY_LLAMA_PARALLELISM_ENV] = "3";
  assert.equal(
    resolveLocalEmbeddingParallelismOverride({ legacyLlama: true }),
    undefined,
  );
  assert.ok(warnings[0].includes(LOCAL_EMBEDDING_CONCURRENCY_ENV));

  delete process.env[LOCAL_EMBEDDING_CONCURRENCY_ENV];
  process.env[LEGACY_LLAMA_PARALLELISM_ENV] = "2.5";
  assert.equal(
    resolveLocalEmbeddingParallelismOverride({ legacyLlama: true }),
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
