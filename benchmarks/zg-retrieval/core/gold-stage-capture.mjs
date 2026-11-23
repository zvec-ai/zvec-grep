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
// Return malformed metadata as an Error value for the diagnostic scorer; do not
// throw here and invalidate an otherwise valid public search response.
export function readStageTrace(response) {
  if (response?.isError) return null;
  const metadata = response?._meta;
  if (!metadata || !Object.hasOwn(metadata, "zg_stage_trace")) return null;
  const traces = metadata.zg_stage_trace;
  if (!Array.isArray(traces))
    return new Error("stage metadata must be an array");
  if (traces.length !== 1)
    return new Error("expected one stage capture for Q1");
  if (traces[0]?.group_id !== "Q1")
    return new Error("unexpected stage capture group; expected Q1");
  return traces[0];
}
