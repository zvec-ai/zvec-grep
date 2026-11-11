import assert from "node:assert/strict";
import test from "node:test";
import { summarizeMeasurements } from "../metrics/measurements.mjs";

const row = (latency, repetition, bytes) => ({
  status: "scored",
  execution_status: "success",
  latency_ms: latency,
  repetition,
  visible_output_bytes: bytes,
});

test("measurements use all successful calls for latency and only the quality call for size", () => {
  assert.deepEqual(
    summarizeMeasurements([
      row(10, 1, 9000),
      row(100, 2, 9000),
      row(20, 5, 2048),
      row(30, 5, 4096),
    ]),
    {
      latency_ms_mean: 40,
      latency_ms_p50: 25,
      latency_sample_count: 4,
      latency_ms_p95: 100,
      latency_ms_p99: 100,
      output_bytes_mean: 3072,
      output_sample_count: 2,
    },
  );
});

test("failures and invalid calls cannot improve measurements; missing values are unavailable", () => {
  const observations = [
    { ...row(0, 5, 0), execution_status: "product_error" },
    { ...row(0, 5, 0), status: "harness_invalid" },
    row(null, 5, null),
    row(-1, 5, -1),
    row(Number.NaN, 5, 1.5),
  ];
  assert.deepEqual(summarizeMeasurements(observations), {
    latency_ms_mean: null,
    latency_ms_p50: null,
    latency_sample_count: 0,
    latency_ms_p95: null,
    latency_ms_p99: null,
    output_bytes_mean: null,
    output_sample_count: 0,
  });
  assert.deepEqual(summarizeMeasurements([...observations, row(0, 5, 0)]), {
    latency_ms_mean: 0,
    latency_ms_p50: 0,
    latency_sample_count: 1,
    latency_ms_p95: 0,
    latency_ms_p99: 0,
    output_bytes_mean: 0,
    output_sample_count: 1,
  });
});
