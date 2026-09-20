import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { performance } from "node:perf_hooks";
import { setTimeout } from "node:timers/promises";

const mainRoot = requiredEnv("ZG_MODEL_BENCH_MAIN_ROOT");
const { createEmbeddingModel } = await import(
  pathToFileURL(join(mainRoot, "dist", "index.js")).href
);

if (envFlag("ZG_MODEL_BENCH_BASELINE")) {
  globalThis.gc?.();
  await setTimeout(250);
  console.log(
    `ZG_MODEL_BENCH_JSON=${JSON.stringify({ implementation: "main", mode: "baseline" })}`,
  );
  process.exit(0);
}

const modelReference =
  process.env.ZG_MODEL_BENCH_MODEL ?? "local/potion-code-16m-v2";
const device = process.env.ZG_MODEL_BENCH_DEVICE;
const batchSize = envInteger("ZG_MODEL_BENCH_BATCH", 256);
const concurrency = envInteger("ZG_MODEL_BENCH_CONCURRENCY", 1);
const vectorsPerRound = envInteger("ZG_MODEL_BENCH_VECTORS", 16_384);
const rounds = envInteger("ZG_MODEL_BENCH_ROUNDS", 5);
const warmupWaves = envInteger("ZG_MODEL_BENCH_WARMUP_WAVES", 2);
const vectorsPerWave = batchSize * concurrency;
if (
  batchSize <= 0 ||
  concurrency <= 0 ||
  rounds <= 0 ||
  vectorsPerRound % vectorsPerWave !== 0
) {
  throw new Error(
    "batch, concurrency and rounds must be positive; vectors must be divisible by batch * concurrency",
  );
}

const model = createEmbeddingModel(modelReference, {
  apiKey: process.env.DASHSCOPE_API_KEY,
  modelCacheDir: process.env.ZG_MODEL_BENCH_CACHE,
  ...(device ? { device } : {}),
});
if (batchSize > model.info.limits.maxBatchSize) {
  throw new Error(
    `batch size ${batchSize} exceeds max ${model.info.limits.maxBatchSize}`,
  );
}
const batch = benchmarkBatch(batchSize);

for (let index = 0; index < warmupWaves; index++) {
  await runWave(model, batch, concurrency);
}
globalThis.gc?.();
await setTimeout(100);
const loadedRssBytes = process.memoryUsage().rss;

const wavesPerRound = vectorsPerRound / vectorsPerWave;
const elapsedSeconds = [];
let checksum = 0;
for (let round = 0; round < rounds; round++) {
  const started = performance.now();
  for (let wave = 0; wave < wavesPerRound; wave++) {
    checksum += await runWave(model, batch, concurrency);
  }
  elapsedSeconds.push((performance.now() - started) / 1_000);
}
await model.dispose();

const requestsPerRound = wavesPerRound * concurrency;
console.log(
  `ZG_MODEL_BENCH_JSON=${JSON.stringify({
    implementation: "main",
    mode: "model",
    model: modelReference,
    device,
    batch_size: batchSize,
    concurrency,
    vectors_per_round: vectorsPerRound,
    rounds,
    warmup_waves: warmupWaves,
    loaded_rss_bytes: loadedRssBytes,
    elapsed_seconds: elapsedSeconds,
    vectors_per_second: elapsedSeconds.map(
      (seconds) => vectorsPerRound / seconds,
    ),
    requests_per_second: elapsedSeconds.map(
      (seconds) => requestsPerRound / seconds,
    ),
    checksum,
  })}`,
);

async function runWave(runtime, contents, parallelism) {
  const results = await Promise.all(
    Array.from({ length: parallelism }, () => runtime.embed(contents)),
  );
  let value = 0;
  for (const result of results) {
    for (const vector of result.vectors) {
      value += (vector[0] ?? 0) + (vector.at(-1) ?? 0);
    }
  }
  return value;
}

function benchmarkBatch(size) {
  return Array.from({ length: size }, (_, index) => ({
    kind: "text",
    text: `pub fn benchmark_${index}(value: usize) -> usize { let adjusted = value.wrapping_mul(31).wrapping_add(${index}); adjusted ^ 0x5a5a }`,
  }));
}

function requiredEnv(name) {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is required`);
  }
  return value;
}

function envInteger(name, fallback) {
  const value = process.env[name];
  if (value === undefined) {
    return fallback;
  }
  const parsed = Number.parseInt(value, 10);
  if (!Number.isSafeInteger(parsed)) {
    throw new Error(`invalid ${name}=${JSON.stringify(value)}`);
  }
  return parsed;
}

function envFlag(name) {
  const value = process.env[name];
  return value !== undefined && value !== "0" && value !== "false";
}
