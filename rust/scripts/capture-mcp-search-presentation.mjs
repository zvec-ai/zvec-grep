// Run after the root npm build to capture the Node.js public MCP contract.
import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { Client, InMemoryTransport } from "@modelcontextprotocol/client";
import { createZvecGrepMcpServer } from "../../dist/mcp/tools.js";

const root = resolve("test/fixtures/repository");
const textRange = (start, end) => ({
  kind: "text",
  start_line: start,
  end_line: end,
  start_byte_offset: start * 100,
  end_byte_offset: end * 100 + 1,
  start_byte_column: 0,
  end_byte_column: 1,
});
const item = (overrides = {}) => ({
  kind: "indexed_entity",
  rank: 1,
  absolute_path: "workspace/src/sample.ts",
  relative_path: "src/sample.ts",
  range: textRange(10, 33),
  excerpt_range: null,
  content: Array.from({ length: 24 }, (_, i) =>
    i === 1
      ? `long-line-${"x".repeat(1000)}-end`
      : `retrieved-source-line-${i + 1}`,
  ).join("\n"),
  content_role: "source",
  status: "fresh",
  score: 0.8,
  matched_by: "fts+vector",
  metadata: null,
  entity_id: null,
  container: null,
  trace: null,
  query_groups: [],
  selection_reason: null,
  coverage_group: null,
  ...overrides,
  content_range:
    overrides.content_range ?? overrides.range ?? textRange(10, 33),
});
const group = (id, query, role = "primary") => ({ id, query, role });
const groups = [
  group("q1", "call chain"),
  group("q2", "refresh"),
  group("fts1", "Runtime", "supplemental"),
];
const result = (items, overrides = {}) => ({
  query: "call chain",
  freshness: "fresh",
  background_refresh: null,
  root: "workspace",
  source: "index",
  coverage: "ranked_sample",
  workspace_index: null,
  items,
  group_results: [],
  diagnostics: {
    empty_reason: null,
    index: null,
    rg: null,
    structure: null,
    timings: [],
  },
  ...overrides,
});
const cases = [
  {
    id: "long-source-and-rank-order",
    result: result([item({ rank: 2 }), item()]),
  },
  {
    id: "groups-outline-and-matched-window",
    result: result(
      [
        item({
          selection_reason: "coverage",
          coverage_group: "q1",
          excerpt_range: textRange(20, 21),
          query_groups: [
            { ...groups[0], rank: 2, matched_by: "vector" },
            { ...groups[2], rank: 1, matched_by: "fts" },
          ],
          outline: Array.from(
            { length: 9 },
            (_, i) => `  outline-member-${i + 1}`,
          ).join("\n"),
          metadata: {
            kind: "code",
            symbol_type: "function",
            symbol_name: "resolveContext",
            scope: "Workspace",
            signature: null,
            documentation: null,
          },
        }),
        item({
          rank: 2,
          relative_path: "notes/design.md",
          range: { kind: "file" },
          content: "# Design\r\n\r\nRetain local evidence.\r\n\r\n",
          selection_reason: "global_fill",
          metadata: {
            kind: "markdown",
            heading: "Design",
            level: 1,
            scope: "Search / Design",
          },
        }),
      ],
      {
        freshness: "possibly_stale",
        background_refresh: "running (12/20)",
        diagnostics: {
          empty_reason: null,
          index: { hits_returned: 2, query_groups: groups, routes: [] },
          rg: null,
          structure: null,
          timings: [],
        },
      },
    ),
  },
  {
    id: "excerpt-only-source-and-redundant-symbol",
    result: result([
      item({
        range: textRange(1, 100),
        excerpt_range: textRange(50, 52),
        content_range: textRange(50, 52),
        content: "function resolveContext() {\r\n  return true;\r\n}\r\n",
        metadata: {
          kind: "code",
          symbol_type: "function",
          symbol_name: "resolveContext",
          scope: null,
          signature: null,
          documentation: null,
        },
      }),
    ]),
  },
  {
    id: "half-open-range",
    result: result([
      item({
        range: { ...textRange(10, 13), end_byte_column: 0 },
        content: "first\nsecond\nthird\n",
      }),
    ]),
  },
  {
    id: "byte-range-and-empty-source",
    result: result([
      item({
        range: { kind: "byte", start_offset: 3, end_offset: 9 },
        content: "fragment",
      }),
      item({ rank: 2, content: "", outline: "available outline only" }),
    ]),
  },
  {
    id: "unicode-and-final-carriage-return",
    result: result([
      item({
        content: `${"界".repeat(180)}\r\n\tindent🙂\n\nlast\r`,
        range: textRange(10, 13),
      }),
    ]),
  },
  {
    id: "long-matched-window",
    result: result([item({ excerpt_range: textRange(20, 31) })]),
  },
  {
    id: "matched-window-near-end",
    result: result([item({ excerpt_range: textRange(32, 33) })]),
  },
  {
    id: "stale-hit",
    result: result([item({ content: "changed", status: "possibly_stale" })], {
      freshness: "possibly_stale",
    }),
  },
  {
    id: "empty-stale-index",
    result: result([], {
      freshness: "possibly_stale",
      background_refresh: "queued",
    }),
  },
  {
    id: "no-searchable-files",
    result: result([], {
      diagnostics: {
        empty_reason: "no_searchable_files",
        index: null,
        rg: null,
        structure: null,
        timings: [],
      },
    }),
  },
];

// Convert representations only; the actual Node.js handler produces expected text.
const camel = (value) => {
  if (Array.isArray(value)) return value.map(camel);
  if (value === null) return undefined;
  if (typeof value !== "object") return value;
  return Object.fromEntries(
    Object.entries(value).map(([key, entry]) => [
      key.replace(/_([a-z])/g, (_, letter) => letter.toUpperCase()),
      camel(entry),
    ]),
  );
};
const nodeRange = (range) =>
  range && {
    ...camel(range),
    ...(range.kind === "text"
      ? {
          startOffset: range.start_byte_offset,
          endOffset: range.end_byte_offset,
          endLine:
            range.end_line -
            Number(
              range.end_byte_column === 0 &&
                range.start_byte_offset < range.end_byte_offset,
            ),
        }
      : {}),
  };
const nodeItem = (value) => ({
  ...camel(value),
  file: {
    absolutePath: resolve(root, value.relative_path),
    relativePath: value.relative_path,
  },
  range: nodeRange(value.range),
  excerptRange: nodeRange(value.excerpt_range),
  ...(value.container
    ? {
        container: {
          ...camel(value.container),
          range: nodeRange(value.container.range),
        },
      }
    : {}),
});

for (const fixture of cases) {
  const value = fixture.result;
  const refresh = value.background_refresh?.match(
    /^(\w+)(?: \((\d+)\/(\d+)\))?$/,
  );
  const response = {
    root,
    freshness: value.freshness,
    indexing: refresh
      ? {
          state: refresh[1],
          completed: refresh[2] ? Number(refresh[2]) : undefined,
          total: refresh[3] ? Number(refresh[3]) : undefined,
        }
      : undefined,
    result: { ...camel(value), root, items: value.items.map(nodeItem) },
  };
  const server = createZvecGrepMcpServer(
    { search: async () => response },
    "fixture",
  );
  const client = new Client({
    name: "search-presentation-fixture",
    version: "1",
  });
  const [clientTransport, serverTransport] =
    InMemoryTransport.createLinkedPair();
  await Promise.all([
    client.connect(clientTransport),
    server.connect(serverTransport),
  ]);
  try {
    for (const preview of ["short", "full"]) {
      const reply = await client.callTool({
        name: "zvec_grep_search",
        arguments: { root, query: "call chain", preview },
      });
      assert.equal(reply.isError, undefined);
      fixture[`expected_${preview}`] = reply.content[0].text;
    }
  } finally {
    await Promise.all([client.close(), server.close()]);
  }
}

await writeFile(
  new URL("../compat/mcp/search-presentation.json", import.meta.url),
  `${JSON.stringify({ schema_version: 1, cases }, null, 2)}\n`,
);
console.log(`Captured ${cases.length} public MCP search cases from Node.js.`);
