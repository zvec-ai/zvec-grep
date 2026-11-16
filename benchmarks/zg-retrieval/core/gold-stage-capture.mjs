export const goldPaths = (pair) => [
  ...new Set(
    [...pair.file_targets, ...pair.entity_targets].map(({ path }) => path),
  ),
];

// Set these before starting the isolated daemon. Only paths are sent to ZG;
// targets, matching rules and metric computation belong to the benchmark.
export function stageTraceEnvironment(enabled, pairs = []) {
  return {
    ZG_TRACE_STAGES: enabled ? "1" : "0",
    ZG_TRACE_PATHS: JSON.stringify(
      enabled ? [...new Set(pairs.flatMap(goldPaths))].sort() : [],
    ),
  };
}

// MCP already binds this metadata to its request. No control files, sequence
// numbers or separate capture files are needed. Frozen suites use one group.
export function readStageTrace(response) {
  if (response?.isError) return null;
  const traces = response?._meta?.zg_stage_trace;
  if (
    !Array.isArray(traces) ||
    traces.length !== 1 ||
    traces[0]?.group_id !== "Q1"
  )
    return null;
  return traces[0];
}
