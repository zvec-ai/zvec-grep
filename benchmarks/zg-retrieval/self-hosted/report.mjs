import { summarizePerformance } from "../reports/performance.mjs";
import assert from "node:assert/strict";

const SUITES = {
  zg: "SWE-QA20",
  beir: "BEIR",
  duretrieval: "DuRetrieval",
  quarry: "Quarry",
};
const MODES = ["hybrid", "fts", "vector"];
const MIB = 1024 ** 2;

export function validatePerformanceCoverage(reports) {
  for (const [name, phases, calls] of [
    ["zg", 11, 100],
    ["beir", 6, 100],
    ["duretrieval", 1, 50],
    ["quarry", 20, 100],
  ]) {
    const groups =
      name === "zg"
        ? reports[name]?.repositories?.map((r) => r.performance)
        : reports[name]?.performance_groups;
    assert.equal(groups?.length, phases, `${name}: missing index groups`);
    const summary = summarizePerformance(groups);
    assert.equal(
      summary.index.index_bytes.count,
      phases,
      `${name}: missing index sizes`,
    );
    for (const [stage, measured] of [
      ["index", summary.index],
      ...Object.entries(summary.query),
    ]) {
      assert.equal(
        measured.measured,
        phases,
        `${name}/${stage}: missing phases`,
      );
      assert.equal(measured.failed, 0, `${name}/${stage}: failed phases`);
      assert.equal(
        measured.memory_measured,
        phases,
        `${name}/${stage}: unavailable RSS`,
      );
      assert.deepEqual(
        measured.concurrency,
        [4],
        `${name}/${stage}: different concurrency`,
      );
      if (stage !== "index")
        assert.equal(
          measured.latency_ms.count,
          calls,
          `${name}/${stage}: missing RT samples`,
        );
    }
  }
}

export function pairedCell(a, b, divisor = 1) {
  if (![a, b].every(Number.isFinite)) return "—";
  const delta =
    a === 0 ? "N/A" : `${b > a ? "+" : ""}${(((b - a) / a) * 100).toFixed(1)}%`;
  return `${(a / divisor).toFixed(2)} / ${(b / divisor).toFixed(2)} (${delta})`;
}

export function performanceReport(bundles, metadata, comparison) {
  const lines = [
    "# Retrieval-only: Node 0.2.2 / Rust canary",
    "",
    `Status: **${comparison.status}**. Each cell is Node / Rust (Rust relative change).`,
    "",
    `Node: \`${metadata.node.version}\`; Rust canary resolved to \`${metadata.rust.version}\`.`,
    "",
    `Harness: \`${metadata.harness_commit}\`. Node runtime: \`${metadata.node_runtime}\`.`,
    "",
    `Hardware: ${metadata.hardware.cpu_model}; ${metadata.hardware.logical_cpus} logical CPUs; ${(metadata.hardware.memory_bytes / 1024 ** 3).toFixed(1)} GiB RAM; ${metadata.hardware.platform}/${metadata.hardware.arch}.`,
    "",
    "Both products are installed globally in separate prefixes from registry.npmjs.org. All suites and versions run sequentially on this runner, using fresh indexes and the same prewarmed model cache. Index embedding and query in-flight limits are both 4; each original query is called five times in hybrid, fts and vector order.",
    "",
    "Coverage follows the supplied profile: SWE-QA20 (11 indexes, 20 queries), BEIR (6 indexes, 20 queries), DuRetrieval (1 index, 10 queries), Quarry (20 indexes, 20 queries).",
    "",
  ];
  if (comparison.status !== "success")
    lines.push(
      "**Incomplete or invalid run: these are diagnostic measurements; a complete version comparison is unavailable.**",
      "",
      ...(comparison.errors ?? []).map(
        (e) => `- ${String(e).replaceAll("\n", " ")}`,
      ),
      "",
    );
  const summaries = {};
  for (const [name] of Object.entries(SUITES)) {
    summaries[name] = {};
    for (const arm of ["node", "rust"]) {
      const report = bundles[arm]?.reports?.[name];
      summaries[name][arm] =
        name === "zg"
          ? summarizePerformance(
              (report?.repositories ?? []).map((r) => r.performance ?? {}),
            )
          : summarizePerformance(report?.performance_groups ?? []);
    }
  }
  const table = (title, headers, rows) =>
    lines.push(
      `## ${title}`,
      "",
      `| ${headers.join(" | ")} |`,
      `| ${headers.map(() => "---").join(" | ")} |`,
      ...rows.map((row) => `| ${row.join(" | ")} |`),
      "",
    );
  const pair = (suite, select, divisor = 1) =>
    pairedCell(
      select(summaries[suite].node),
      select(summaries[suite].rust),
      divisor,
    );
  const counts = (suite, select) =>
    `${select(summaries[suite].node)} / ${select(summaries[suite].rust)}`;
  const names = Object.entries(SUITES);
  table(
    "1. Index time (s)",
    ["Dataset", "Phases Node/Rust", "Total", "Mean", "p95", "p99"],
    names.map(([s, label]) => [
      label,
      counts(s, (p) => p.index.measured),
      ...["total", "mean", "p95", "p99"].map((k) =>
        pair(s, (p) => p.index.total_ms[k], 1000),
      ),
    ]),
  );
  table(
    "2. Query request RT (ms)",
    ["Dataset", "Mode", "Samples Node/Rust", "Mean", "p50", "p95", "p99"],
    names.flatMap(([s, label]) =>
      MODES.map((mode) => [
        label,
        mode,
        counts(s, (p) => p.query[mode].latency_ms.count),
        ...["mean", "p50", "p95", "p99"].map((k) =>
          pair(s, (p) => p.query[mode].latency_ms[k]),
        ),
      ]),
    ),
  );
  table(
    "3. Query group time (s)",
    ["Dataset", "Mode", "Phases Node/Rust", "Total", "Mean", "p95", "p99"],
    names.flatMap(([s, label]) =>
      MODES.map((mode) => [
        label,
        mode,
        counts(s, (p) => p.query[mode].measured),
        ...["total", "mean", "p95", "p99"].map((k) =>
          pair(s, (p) => p.query[mode].total_ms[k], 1000),
        ),
      ]),
    ),
  );
  table(
    "4. Index / Query RSS (MiB)",
    [
      "Dataset",
      "Stage",
      "Samples Node/Rust",
      "Mean",
      "p95",
      "p99",
      "Sampled peak",
    ],
    names.flatMap(([s, label]) =>
      ["index", ...MODES].map((stage) => {
        const select = (p) =>
          stage === "index"
            ? p.index.memory_rss_bytes
            : p.query[stage].memory_rss_bytes;
        return [
          label,
          stage === "index" ? "Index" : `Query ${stage}`,
          counts(s, (p) => select(p).count),
          ...["mean", "p95", "p99", "max"].map((k) =>
            pair(s, (p) => select(p)[k], MIB),
          ),
        ];
      }),
    ),
  );
  table(
    "5. Index size (bytes)",
    [
      "Dataset",
      "Indexes Node/Rust",
      "Node total bytes",
      "Rust total bytes",
      "Node / Rust (change)",
    ],
    names.map(([s, label]) => [
      label,
      counts(s, (p) => p.index.index_bytes.count),
      summaries[s].node.index.index_bytes.total,
      summaries[s].rust.index.index_bytes.total,
      pair(s, (p) => p.index.index_bytes.total),
    ]),
  );
  table(
    "Index size distribution (MiB)",
    ["Dataset", "Total", "Mean", "p95", "p99"],
    names.map(([s, label]) => [
      label,
      ...["total", "mean", "p95", "p99"].map((k) =>
        pair(s, (p) => p.index.index_bytes[k], MIB),
      ),
    ]),
  );
  lines.push(
    "## Measurement scope",
    "",
    "Query RT covers the public MCP call before response parsing and scoring. Phase time includes startup/model loading/final optimization for indexing and response capture for query groups. Model downloads are prewarmed outside measured phases. RSS sums product processes and the detached daemon at a target 100 ms sampling interval; shared pages may be double-counted and brief peaks can be missed. Percentiles pool raw samples: p50 is median; p95/p99 use nearest rank. Index size sums regular files under .zvec-grep; model caches and symlinks are excluded.",
    "",
    "Fixed query order retains cold-session and cache effects. These measurements describe these published versions on this machine, including their native batching and storage behavior. Missing values are unavailable, not zero. Raw reports retain failed phases and coverage.",
    "",
    "Full retrieval quality comparison: [quality.md](quality.md). Machine-readable results: summary.json, comparison.json, identities.json and host-samples.json. Package identities include registry SHA-512, tarball SHA-256 and executable SHA-256.",
    "",
  );
  return lines.join("\n");
}
