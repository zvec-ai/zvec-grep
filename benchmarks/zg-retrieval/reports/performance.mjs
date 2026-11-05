import { statistics } from "../metrics/statistics.mjs";

/** Recompute from raw samples instead of averaging per-index percentiles. */
export function summarizePerformance(groups) {
  const summarize = (stages) => {
    const present = stages.filter(Boolean);
    const memory = present.flatMap((stage) => stage.memory?.samples ?? []);
    return {
      planned: stages.length,
      measured: present.length,
      failed: present.filter(
        (stage) => stage.status === "failed" || stage.failed_calls > 0,
      ).length,
      concurrency: [...new Set(present.map((stage) => stage.concurrency))],
      total_ms: statistics(present.map((stage) => stage.total_ms)),
      memory_rss_bytes: statistics(memory.map((sample) => sample.rss_bytes)),
      memory_measured: present.filter(
        (stage) => stage.memory?.status === "available",
      ).length,
    };
  };
  const indexes = groups.map((group) => group.index);
  return {
    schema_version: 1,
    percentile_method: "p50 median; p95/p99 nearest-rank",
    index: {
      ...summarize(indexes),
      index_bytes: statistics(indexes.map((stage) => stage?.index_bytes)),
    },
    query: Object.fromEntries(
      ["hybrid", "fts", "vector"].map((mode) => [
        mode,
        {
          ...summarize(groups.map((group) => group.query?.[mode])),
          latency_ms: statistics(
            groups.flatMap((group) => group.query?.[mode]?.samples_ms ?? []),
          ),
        },
      ]),
    ),
  };
}

const number = (value, divisor = 1) =>
  Number.isFinite(value) ? (value / divisor).toFixed(2) : "—";
const triple = (stats, divisor = 1) =>
  ["mean", "p95", "p99"]
    .map((key) => number(stats?.[key], divisor))
    .join(" / ");
const queryRt = (stats) =>
  ["mean", "p50", "p95", "p99"].map((key) => number(stats?.[key])).join(" / ");

export function markdownPerformance(summary) {
  if (!summary) return "";
  const rows = [
    ["Index", summary.index],
    ...Object.entries(summary.query).map(([mode, stage]) => [
      `Query ${mode}`,
      stage,
    ]),
  ];
  return [
    "### Index & Query performance",
    "",
    "| Stage | Measured phases | Failed phases | Concurrency limit | Time samples | Total (s) | Phase time mean / p95 / p99 (s) |",
    "| --- | ---: | ---: | --- | ---: | ---: | --- |",
    ...rows.map(
      ([label, stage]) =>
        `| ${label} | ${stage.measured}/${stage.planned} | ${stage.failed} | ${stage.concurrency.join(", ") || "—"} | ${stage.total_ms.count} | ${number(stage.total_ms.total, 1000)} | ${triple(stage.total_ms, 1000)} |`,
    ),
    "",
    "| Query mode | RT samples | RT mean / p50 / p95 / p99 (ms) |",
    "| --- | ---: | --- |",
    ...Object.entries(summary.query).map(
      ([mode, stage]) =>
        `| ${mode} | ${stage.latency_ms.count} | ${queryRt(stage.latency_ms)} |`,
    ),
    "",
    "| Stage | Complete RSS phases | RSS samples | RSS mean / p95 / p99 (MiB) | Sampled peak RSS (MiB) |",
    "| --- | ---: | ---: | --- | ---: |",
    ...rows.map(
      ([label, stage]) =>
        `| ${label} | ${stage.memory_measured}/${stage.planned} | ${stage.memory_rss_bytes.count} | ${triple(stage.memory_rss_bytes, 1024 ** 2)} | ${number(stage.memory_rss_bytes.max, 1024 ** 2)} |`,
    ),
    "",
    `Generated index size: **${number(summary.index.index_bytes.total, 1024 ** 2)} MiB total** across ${summary.index.index_bytes.count}/${summary.index.planned} indexes; mean / p95 / p99: **${triple(summary.index.index_bytes, 1024 ** 2)} MiB**.`,
    "",
    `Index total includes CLI startup, model loading/download and final optimization. Query RT covers all attempted public MCP calls, including failures; phase totals include response capture. Phase-time percentiles describe individual corpus/mode phases; totals sum those phases and are not overall parallel workflow duration.`,
    "",
    "RSS is sampled every 100 ms across product processes and the resident daemon, excluding the benchmark driver; shared pages may be counted more than once. RSS percentiles pool time samples, and sampled peaks may miss brief spikes. Index bytes sum regular files under .zvec-grep after building, excluding model cache and symlinks. p50 uses the median; p95/p99 use nearest rank; — means unavailable. Index embedding concurrency is capped at 4 (the product can reduce it on throttling); public queries use up to 4 in-flight requests. The storage engine retains its own commit scheduling.",
    "",
  ].join("\n");
}
