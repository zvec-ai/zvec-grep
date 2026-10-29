// Run after npm build; stdout contains the shared CLI presentation fixtures.
import { readFile } from "node:fs/promises";
import { formatCliContextResult } from "../../dist/cli/format/context.js";
import { nodeResult } from "../../test/fixtures/cli-presentation.mjs";

const source = JSON.parse(
  await readFile(
    new URL("../compat/mcp/search-presentation.json", import.meta.url),
    "utf8",
  ),
);
const ids = [
  "long-source-and-rank-order",
  "groups-outline-and-matched-window",
  "excerpt-only-source-and-redundant-symbol",
  "empty-stale-index",
  "no-searchable-files",
];
const cases = source.cases
  .filter((fixture) => ids.includes(fixture.id))
  .map(({ id, result }) => ({ id, result }));
const detailed = cases.find(
  (fixture) => fixture.id === "groups-outline-and-matched-window",
);
detailed.result.items[0].metadata.signature =
  "function resolveContext(query: string): ContextResult";
detailed.result.items[0].outline =
  "function resolveContext(query: string) {\n\ncalls: loadIndex, search";
const grouped = structuredClone(detailed);
grouped.id = "query-groups-with-empty-group";
grouped.result.workspace_index = {
  root: "workspace",
  path: "workspace/.zvec-grep",
};
grouped.result.diagnostics.index.routes = [
  { id: "fts1", mode: "fts", query: "refresh" },
];
grouped.result.group_results = [
  { id: "q0", query: "missing", role: "primary", items: [] },
  {
    id: "q1",
    query: "call chain",
    role: "primary",
    items: grouped.result.items,
  },
  {
    id: "fts1",
    query: "refresh",
    role: "supplemental",
    items: [grouped.result.items[0]],
  },
];
cases.push(grouped);
const traced = structuredClone(
  cases.find(
    (fixture) => fixture.id === "excerpt-only-source-and-redundant-symbol",
  ),
);
traced.id = "trace-with-found-and-missing-routes";
traced.trace = true;
traced.result.items[0].trace = {
  recall: [
    {
      path: "fts",
      route_id: "fts1",
      query: "call chain",
      found: true,
      rank: 2,
      score: 0.8,
      forced: false,
      reason: null,
    },
    {
      path: "vector",
      route_id: "vector",
      query: "call chain",
      found: false,
      rank: null,
      score: null,
      forced: false,
      reason: null,
    },
  ],
  fusion: { rank: 1, score: 0.8, forced: true },
  ranking: null,
  final_selection: { returned_by_limit: true, cutoff_rank: 7 },
};
cases.push(traced);
for (const fixture of cases) {
  if (fixture.result.diagnostics.index) {
    fixture.result.diagnostics.index.query_groups = [];
  }
  // CLI groups do not display MCP coverage-selection annotations.
  for (const item of fixture.result.items) {
    item.query_groups = [];
    item.selection_reason = null;
    item.coverage_group = null;
  }
  const normalized = nodeResult(fixture.result);
  for (const preview of ["none", "short", "full", "detail"]) {
    fixture[`expected_${preview}`] = formatCliContextResult(normalized, {
      preview,
      color: "never",
      trace: fixture.trace === true,
    });
  }
}
process.stdout.write(
  `${JSON.stringify({ schema_version: 1, cases }, null, 2)}\n`,
);
