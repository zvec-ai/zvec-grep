import assert from "node:assert/strict";
import test from "node:test";
import { mergeLiveAndIndexedResults } from "../../dist/cli/live-index-merge.js";
import {
  formatCliContextResult,
  printCliContextResult,
} from "../../dist/cli/format/context.js";

const range = (startLine, endLine = startLine) => ({
  kind: "text",
  startLine,
  endLine,
  startOffset: 0,
  endOffset: 30,
});
const file = (name) => ({ absolutePath: `/repo/${name}`, relativePath: name });
const lexical = (name, rank, line, extra = {}) => ({
  kind: "lexical_match",
  rank,
  file: file(name),
  range: range(line),
  content: "connection pool current",
  status: "fresh",
  matchedBy: "lexical",
  ...extra,
});
const indexed = (name, rank, line, extra = {}) => ({
  kind: "indexed_entity",
  rank,
  file: file(name),
  range: range(line),
  entityId: `${name}:${line}`,
  content: "reuse database connections",
  contentRole: "source",
  status: "fresh",
  matchedBy: "vector",
  score: 0.01,
  ...extra,
});
const result = (source, items, extra = {}) => ({
  query: "connection pool",
  root: "/repo",
  source,
  coverage: source === "rg" ? "rg_exhaustive" : "ranked_sample",
  items,
  diagnostics:
    source === "rg"
      ? {}
      : { index: { hitsReturned: items.length, routes: [] } },
  ...extra,
});

test("current matches precede fresh semantic results; stale source never consumes the limit", () => {
  const index = result("index", [
    indexed("changed.ts", 1, 1, { status: "possibly_stale" }),
    indexed("deleted.ts", 2, 1, { status: "possibly_stale" }),
    indexed("stable.ts", 3, 1),
  ]);
  const live = result("rg", [
    lexical("added.ts", 2, 1),
    lexical("changed.ts", 1, 2),
  ]);
  const before = structuredClone({ index, live });
  const merged = mergeLiveAndIndexedResults(index, live);
  assert.equal(merged.staleItemsOmitted, 2);
  assert.deepEqual(
    merged.result.items.map((item) => item.file.relativePath),
    ["changed.ts", "added.ts", "stable.ts"],
  );
  assert.deepEqual(
    merged.result.items.map((item) => item.rank),
    [1, 2, 3],
  );
  assert.deepEqual(
    merged.result.items.map((item) => item.matchedBy),
    ["lexical", "lexical", "vector"],
  );
  assert.equal(merged.result.items[0].score, undefined);
  assert.equal(merged.result.coverage, "ranked_sample");
  assert.equal(merged.result.diagnostics.index.hitsReturned, 1);
  assert.equal(
    mergeLiveAndIndexedResults(index, live, 1).result.items.length,
    1,
  );
  assert.deepEqual(
    { index, live },
    before,
    "input snapshots must not be mutated",
  );
});

test("deduplication uses exact symbol ranges, not extraction IDs or enclosing classes", () => {
  const metadata = {
    kind: "code",
    symbolName: "openPool",
    scope: "Pool",
    symbolType: "function",
  };
  const live = result("rg", [
    lexical("pool.ts", 1, 12, {
      metadata,
      container: { entityId: "live:openPool", range: range(10, 15), metadata },
    }),
    lexical("pool.ts", 2, 13, {
      metadata,
      container: { entityId: "live:openPool", range: range(10, 15), metadata },
    }),
  ]);
  const index = result("index", [
    indexed("pool.ts", 1, 10, {
      entityId: "index:openPool",
      range: range(10, 15),
      metadata,
    }),
    indexed("pool.ts", 2, 1, {
      range: range(1, 50),
      contentRole: "outline",
      metadata: { kind: "code", symbolName: "Pool", symbolType: "class" },
    }),
    indexed("pool.ts", 3, 10, {
      range: range(10, 15),
      entityId: "another-symbol",
      metadata: { ...metadata, symbolName: "anotherPool" },
    }),
  ]);
  const merged = mergeLiveAndIndexedResults(index, live).result;
  assert.equal(merged.items.length, 3);
  assert.equal(merged.items[0].kind, "lexical_match");
  assert.equal(merged.items[1].metadata.symbolName, "Pool");
  assert.equal(merged.items[2].metadata.symbolName, "anotherPool");
});

test("raw literal evidence only deduplicates source-backed overlapping matches", () => {
  const live = result("rg", [lexical("notes.txt", 1, 5)]);
  const index = result("index", [
    indexed("notes.txt", 1, 4, {
      range: range(4, 8),
      content: "CONNECTION POOL",
    }),
    indexed("notes.txt", 2, 1, {
      range: range(1, 3),
      content: "connection pool elsewhere",
    }),
    indexed("notes.txt", 3, 4, {
      entityId: "outline",
      range: range(4, 8),
      contentRole: "outline",
      content: "connection pool",
    }),
    indexed("notes.txt", 4, 4, {
      entityId: "unrelated-source",
      range: range(4, 8),
    }),
  ]);
  const merged = mergeLiveAndIndexedResults(index, live).result;
  assert.equal(merged.items.length, 4);
  assert.equal(merged.items[0].kind, "lexical_match");
  const cased = {
    ...live,
    query: "Connection Pool",
    items: [{ ...live.items[0], content: "Connection Pool current" }],
  };
  assert.equal(
    mergeLiveAndIndexedResults({ ...index, query: cased.query }, cased).result
      .items.length,
    5,
  );
});

test("single group rendering includes new files and uses the merged ranks", () => {
  const old = indexed("stable.ts", 1, 1, {
    selectionReason: "coverage",
    coverageGroup: "Q1",
    queryGroups: [
      {
        id: "Q1",
        query: "connection pool",
        role: "primary",
        rank: 1,
        matchedBy: "vector",
      },
    ],
  });
  const index = result("index", [old], {
    groupResults: [
      { id: "Q1", query: "connection pool", role: "primary", items: [old] },
    ],
  });
  const live = result("rg", [lexical("new.ts", 1, 3)], {
    diagnostics: {
      rg: {
        backend: "rg",
        command: "rg",
        args: [],
        ignoredDirectories: [],
        truncated: false,
      },
    },
  });
  const merged = mergeLiveAndIndexedResults(index, live).result;
  assert.equal(merged.groupResults, undefined);
  assert.equal(merged.items[1].selectionReason, undefined);
  assert.equal(merged.items[1].queryGroups, undefined);
  const output = formatCliContextResult(merged, {});
  assert.match(output, /#1 matchedBy=lexical new\.ts:3/);
  assert.match(output, /connection pool current/);
  assert.match(output, /#2 matchedBy=vector stable\.ts:1/);
  const lines = [];
  const previous = console.log;
  try {
    console.log = (...args) => lines.push(args.join(" "));
    printCliContextResult(merged, { human: true, color: "never" });
  } finally {
    console.log = previous;
  }
  assert.match(lines.join("\n"), /current files \+ workspace index/);
  assert.match(lines.join("\n"), /connection pool current/);
});

test("empty live results do not endorse stale semantic evidence", () => {
  const index = result("index", [
    indexed("removed.ts", 1, 1, { status: "possibly_stale" }),
  ]);
  const merged = mergeLiveAndIndexedResults(index, result("rg", []));
  assert.equal(merged.staleItemsOmitted, 1);
  assert.deepEqual(merged.result.items, []);
  assert.equal(merged.result.coverage, "ranked_sample");
});

test("explicit index requests, multi-query groups and different queries are not merged", () => {
  const index = result("index", [indexed("pool.ts", 1, 1)]);
  const live = result("rg", [lexical("added.ts", 1, 2)]);
  assert.equal(mergeLiveAndIndexedResults(index).result, index);
  assert.equal(
    mergeLiveAndIndexedResults(index, { ...live, source: "index" }).result,
    index,
  );
  assert.equal(
    mergeLiveAndIndexedResults(index, { ...live, query: "another question" })
      .result,
    index,
  );
  const multiple = { ...index, groupResults: [{ items: [] }, { items: [] }] };
  assert.equal(mergeLiveAndIndexedResults(multiple, live).result, multiple);
  assert.equal(mergeLiveAndIndexedResults(live, live).result, live);
});
