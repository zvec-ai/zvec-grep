import assert from "node:assert/strict";
import test from "node:test";
import {
  contextWarningLines,
  formatAgentContextResult,
  formatCliContextResult,
  printCliContextResult,
} from "../../dist/cli/format/context.js";
import { mergeLiveAndIndexedResults } from "../../dist/cli/live-index-merge.js";
import { parseServerSearchResponse } from "../../dist/cli/server-search.js";
import {
  zvecGrepCliSearchInputSchema,
  zvecGrepSearchInputSchema,
  zvecGrepSearchOutputSchema,
} from "../../dist/mcp/schemas.js";

const semantic = {
  status: "skipped",
  reason: "preparation_budget_exceeded",
  budgetMs: 300,
};
const incompleteLabel =
  "No local text matches; semantic search exceeded the preparation budget.";
const warning =
  "warning: semantic search exceeded the preparation budget; showing local text matches only (search coverage is incomplete)";

function partialResult() {
  const groups = [
    { id: "Q1", query: "connection pool", role: "primary", items: [] },
    { id: "Q2", query: "PoolManager", role: "supplemental", items: [] },
  ];
  return {
    query: "connection pool",
    root: "/repo",
    source: "index",
    coverage: "ranked_sample",
    items: [],
    groupResults: groups,
    diagnostics: {
      emptyReason: "semantic_incomplete",
      semantic: { ...semantic },
      index: {
        hitsReturned: 0,
        queryGroups: groups.map(({ id, query, role }) => ({ id, query, role })),
        routes: [
          { id: "fts", mode: "fts", query: "connection pool" },
          { id: "fts", mode: "fts", query: "PoolManager" },
        ],
      },
    },
  };
}

test("semantic budget metadata survives the actual CLI server-response schema parser", () => {
  const result = partialResult();
  const response = { root: "/repo", freshness: "fresh", result };
  const parsed = parseServerSearchResponse(response);
  assert.deepEqual(parsed.result.diagnostics, result.diagnostics);
  assert.deepEqual(parsed.result.groupResults, result.groupResults);
  assert.equal(
    parsed.freshness,
    "fresh",
    "freshness is separate from recall completeness",
  );
  for (const invalid of [
    { ...semantic, budgetMs: -1 },
    { ...semantic, status: "complete" },
    { ...semantic, reason: "unknown" },
  ]) {
    assert.equal(
      zvecGrepSearchOutputSchema.safeParse({
        ...response,
        result: {
          ...result,
          diagnostics: { ...result.diagnostics, semantic: invalid },
        },
      }).success,
      false,
    );
  }
});

test("empty primary groups retain incomplete semantics while explicit FTS groups remain local no-matches", () => {
  const result = partialResult();
  const before = structuredClone(result);
  const text = formatCliContextResult(result, { color: "never" });
  const primary = text.slice(
    text.indexOf("Q1 [primary]"),
    text.indexOf("Q2 [supplemental]"),
  );
  const supplemental = text.slice(text.indexOf("Q2 [supplemental]"));
  assert.match(primary, /Q1 \[primary\]: connection pool\nhits: 0/);
  assert.ok(primary.includes(incompleteLabel));
  assert.doesNotMatch(primary, /(?:^|\n)No matches\./);
  assert.match(supplemental, /Q2 \[supplemental\]: PoolManager\nhits: 0/);
  assert.match(supplemental, /No matches\./);
  assert.doesNotMatch(supplemental, /semantic search exceeded/);
  assert.equal(
    formatAgentContextResult(result, { color: "never" }),
    incompleteLabel,
  );
  assert.deepEqual(contextWarningLines(result), [warning]);
  assert.deepEqual(
    result,
    before,
    "formatting cannot rewrite original groups or diagnostics",
  );

  const single = { ...result, groupResults: [result.groupResults[0]] };
  assert.equal(
    formatCliContextResult(single, { color: "never" }),
    incompleteLabel,
  );
});

test("human grouped output does not label unfinished semantic recall as no matches", () => {
  const result = partialResult();
  const lines = [];
  const originalLog = console.log;
  try {
    console.log = (...values) => lines.push(values.join(" "));
    printCliContextResult(result, { human: true, color: "never" });
  } finally {
    console.log = originalLog;
  }
  const text = lines.join("\n");
  const primary = text.slice(
    text.indexOf("Q1 [primary]"),
    text.indexOf("Q2 [supplemental]"),
  );
  const supplemental = text.slice(text.indexOf("Q2 [supplemental]"));
  assert.match(
    primary,
    /Reason:\s+No local text matches; semantic search exceeded/,
  );
  assert.doesNotMatch(primary, /Reason:\s+No matches/);
  assert.match(supplemental, /Reason:\s+No matches/);
});

test("live merge preserves incomplete recall with empty, stale-only and nonempty local results", () => {
  const base = partialResult();
  base.groupResults = [base.groupResults[0]];
  const live = {
    query: base.query,
    root: base.root,
    source: "rg",
    coverage: "rg_exhaustive",
    items: [],
    diagnostics: {},
  };
  const item = {
    kind: "indexed_entity",
    rank: 1,
    file: { absolutePath: "/repo/pool.ts", relativePath: "pool.ts" },
    range: {
      kind: "text",
      startLine: 1,
      endLine: 1,
      startOffset: 0,
      endOffset: 15,
    },
    entityId: "pool",
    content: "connection pool",
    status: "possibly_stale",
    matchedBy: "fts",
  };
  for (const items of [[], [item]]) {
    const indexed = {
      ...base,
      items,
      groupResults: [{ ...base.groupResults[0], items }],
      diagnostics: {
        ...base.diagnostics,
        emptyReason: items.length ? undefined : "semantic_incomplete",
      },
    };
    const merged = mergeLiveAndIndexedResults(indexed, live).result;
    assert.equal(merged.items.length, 0);
    assert.deepEqual(merged.diagnostics.semantic, semantic);
    assert.equal(merged.diagnostics.emptyReason, "semantic_incomplete");
    assert.equal(
      formatCliContextResult(merged, { color: "never" }),
      incompleteLabel,
    );
    assert.deepEqual(contextWarningLines(merged), [warning]);
  }
  const current = {
    ...item,
    kind: "lexical_match",
    status: "fresh",
    matchedBy: "lexical",
    entityId: undefined,
  };
  const merged = mergeLiveAndIndexedResults(base, {
    ...live,
    items: [current],
  }).result;
  assert.equal(merged.items.length, 1);
  assert.equal(merged.items[0].matchedBy, "lexical");
  assert.equal(merged.diagnostics.emptyReason, undefined);
  assert.deepEqual(merged.diagnostics.semantic, semantic);
  assert.deepEqual(contextWarningLines(merged), [warning]);
  assert.doesNotMatch(formatCliContextResult(merged, {}), /No .*matches/);
});

test("budget fallback preserves keyword scan limits alongside semantic incompleteness", () => {
  const base = partialResult();
  base.groupResults = [base.groupResults[0]];
  const keyword = {
    kind: "lexical_match",
    rank: 1,
    file: { absolutePath: "/repo/current.ts", relativePath: "current.ts" },
    range: {
      kind: "text",
      startLine: 1,
      endLine: 1,
      startOffset: 0,
      endOffset: 36,
    },
    content: "export function connectionPool() {}",
    status: "fresh",
    matchedBy: "keyword",
  };
  for (const items of [[], [keyword]]) {
    const keywords = {
      terms: ["connection", "pool"],
      candidates: items.length,
      truncated: true,
    };
    const live = {
      query: base.query,
      root: base.root,
      source: "rg",
      coverage: "ranked_sample",
      items,
      diagnostics: { keywords },
    };
    const before = structuredClone({ base, live });
    const merged = mergeLiveAndIndexedResults(base, live).result;
    assert.deepEqual(merged.diagnostics.semantic, semantic);
    assert.deepEqual(merged.diagnostics.keywords, keywords);
    assert.equal(
      merged.diagnostics.emptyReason,
      items.length ? undefined : "semantic_incomplete",
    );
    assert.deepEqual(contextWarningLines(merged), [
      warning,
      "warning: keyword search reached a scan or resource limit; text coverage is incomplete",
    ]);
    if (items.length) {
      assert.equal(merged.items[0].matchedBy, "keyword");
      assert.match(formatCliContextResult(merged, {}), /matchedBy=keyword/);
    } else {
      assert.equal(formatCliContextResult(merged, {}), incompleteLabel);
    }
    assert.deepEqual({ base, live }, before);
  }
});

test("explicit hybrid wait policy is internal-only and never invented for ordinary searches", () => {
  const input = { root: "/repo", query: "connection pool" };
  assert.equal(
    zvecGrepCliSearchInputSchema.parse(input).semanticPolicy,
    undefined,
  );
  assert.equal(
    zvecGrepCliSearchInputSchema.parse({ ...input, semanticPolicy: "wait" })
      .semanticPolicy,
    "wait",
  );
  assert.equal(
    zvecGrepSearchInputSchema.parse({ ...input, semanticPolicy: "wait" })
      .semanticPolicy,
    undefined,
  );
  assert.equal("semanticPolicy" in zvecGrepSearchInputSchema.shape, false);
  assert.equal(
    zvecGrepCliSearchInputSchema.safeParse({ ...input, semanticPolicy: "auto" })
      .success,
    false,
  );
});
