import assert from "node:assert/strict";
import { statistics } from "./statistics.mjs";

/** Reject incomplete timing evidence before any aggregation can omit it. */
export function validateCallLatency({ latency_ms, execution_status }) {
  assert.ok(
    (Number.isFinite(latency_ms) && latency_ms >= 0) ||
      (execution_status === "product_error" && latency_ms === null),
    "invalid call latency",
  );
}

/** Measurements describe public MCP calls, not indexing time or model tokens. */
export function summarizeMeasurements(
  observations,
  { qualityRepetition = 5 } = {},
) {
  const successful = observations.filter(
    (row) =>
      row.execution_status === "success" && row.status !== "harness_invalid",
  );
  const latencies = successful
    .map((row) => row.latency_ms)
    .filter((value) => Number.isFinite(value) && value >= 0)
    .sort((left, right) => left - right);
  const sizes = successful
    .filter((row) => row.repetition === qualityRepetition)
    .map((row) => row.visible_output_bytes)
    .filter((value) => Number.isSafeInteger(value) && value >= 0);
  const middle = Math.floor(latencies.length / 2);
  const timing = statistics(latencies);
  return {
    latency_ms_mean: latencies.length
      ? latencies.reduce((sum, value) => sum + value, 0) / latencies.length
      : null,
    latency_ms_p50: latencies.length
      ? latencies.length % 2
        ? latencies[middle]
        : (latencies[middle - 1] + latencies[middle]) / 2
      : null,
    latency_sample_count: latencies.length,
    latency_ms_p95: timing.p95,
    latency_ms_p99: timing.p99,
    output_bytes_mean: sizes.length
      ? sizes.reduce((sum, value) => sum + value, 0) / sizes.length
      : null,
    output_sample_count: sizes.length,
  };
}
