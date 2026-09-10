import assert from "node:assert/strict";
import test from "node:test";
import {
  canUseCurrentSource,
  isLocalIndexUnavailable,
} from "../../dist/search/default-policy.js";
import {
  INCOMPATIBLE_SERVER_SEARCH_MESSAGE,
  parseServerSearchResponse,
} from "../../dist/cli/server-search.js";
import {
  contextWarningLines,
  formatAgentContextResult,
  formatCliContextResult,
} from "../../dist/cli/format/context.js";

const ordinary = {
  root: "/repo",
  queries: ["permission validation"],
  routes: [],
  freshness: "eventual",
  autoUpdate: true,
};

test("current-source eligibility preserves explicit recall and freshness intent", () => {
  assert.equal(canUseCurrentSource(ordinary), true);
  assert.equal(canUseCurrentSource({ ...ordinary, autoUpdate: false }), true);
  for (const override of [
    { queries: undefined },
    { queries: [] },
    { queries: [" "] },
    { queries: ["one", "two"] },
    { routes: [{ mode: "fts", query: "needle" }] },
    { routes: [{ mode: "vector", query: "needle" }] },
    { fuse: true },
    { preferSymbol: true },
    { symbolTypes: ["function"] },
    { trace: true },
    { freshness: "wait_for_fresh" },
    { semanticPolicy: "wait" },
  ]) {
    assert.equal(
      canUseCurrentSource({ ...ordinary, ...override }),
      false,
      JSON.stringify(override),
    );
  }
});

test("current-source availability keeps populated, empty, and remote indexes on their original path", () => {
  const empty = {
    filesIndexed: 0,
    filesAdded: 0,
    filesPending: 0,
    filesFailed: 0,
  };
  const local = {
    indexed: true,
    workspaceIndex: { embedding: { provider: "local" } },
  };
  assert.equal(isLocalIndexUnavailable({ indexed: false }), true);
  assert.equal(
    isLocalIndexUnavailable({ ...local, indexPolicy: "disabled" }),
    true,
  );
  assert.equal(isLocalIndexUnavailable(local), false);
  assert.equal(isLocalIndexUnavailable({ ...local, status: empty }), false);
  for (const field of ["filesAdded", "filesPending", "filesFailed"]) {
    const status = { ...empty, [field]: 1 };
    assert.equal(isLocalIndexUnavailable({ ...local, status }), true);
    assert.equal(
      isLocalIndexUnavailable({
        ...local,
        status: { ...status, filesIndexed: 1 },
      }),
      false,
    );
  }
  for (const provider of ["openai", "remote", "custom"]) {
    assert.equal(isLocalIndexUnavailable({ indexed: false }, provider), false);
    assert.equal(
      isLocalIndexUnavailable({ ...local, indexPolicy: "disabled" }, provider),
      false,
      "an active remote provider overrides stale local metadata",
    );
    assert.equal(
      isLocalIndexUnavailable({
        indexed: false,
        workspaceIndex: { embedding: { provider } },
      }),
      false,
    );
  }
});

function currentSource(items = []) {
  return {
    root: "/repo",
    freshness: "fresh",
    result: {
      root: "/repo",
      query: "permission validation",
      source: "rg",
      coverage: "ranked_sample",
      items,
      diagnostics: {
        semantic: { status: "skipped", reason: "index_unavailable" },
        keywords: {
          terms: ["permission", "validation"],
          candidates: 1,
          truncated: false,
        },
        ...(items.length ? {} : { emptyReason: "semantic_incomplete" }),
      },
    },
  };
}

test("server parsing accepts tagged current-source results without losing top-level evidence", () => {
  const response = currentSource([
    {
      kind: "lexical_match",
      rank: 1,
      file: { absolutePath: "/repo/auth.ts", relativePath: "auth.ts" },
      range: {
        kind: "text",
        startLine: 1,
        endLine: 1,
        startOffset: 0,
        endOffset: 24,
      },
      content: "// permission validation",
      status: "fresh",
      matchedBy: "keyword",
    },
  ]);
  const parsed = parseServerSearchResponse(response);
  assert.deepEqual(parsed.result.items, response.result.items);
  assert.deepEqual(parsed.result.diagnostics, response.result.diagnostics);
  assert.equal(parsed.result.groupResults, undefined);
  assert.equal(parsed.indexing, undefined);
});

test("current-source parsing does not admit old ungrouped indexed output or malformed diagnostics", () => {
  for (const changed of [
    { source: "index" },
    { diagnostics: {} },
    {
      diagnostics: {
        semantic: { status: "skipped", reason: "preparation_budget_exceeded" },
      },
    },
    {
      diagnostics: {
        semantic: { status: "complete", reason: "index_unavailable" },
      },
    },
  ]) {
    const response = currentSource();
    response.result = { ...response.result, ...changed };
    assert.throws(() => parseServerSearchResponse(response), {
      message: INCOMPATIBLE_SERVER_SEARCH_MESSAGE,
    });
  }
});

test("current-source empty output distinguishes fresh content from incomplete coverage", () => {
  const { result } = parseServerSearchResponse(currentSource());
  for (const format of [formatAgentContextResult, formatCliContextResult]) {
    const text = format(result, { preview: "short", color: "never" });
    assert.match(text, /No local text matches; semantic index is not ready\./);
    assert.doesNotMatch(text, /No matches\.|exceeded.*budget/);
  }
  assert.deepEqual(contextWarningLines(result), [
    "warning: semantic index is not ready; showing current text matches only (search coverage is incomplete)",
  ]);
  const budget = {
    ...result,
    diagnostics: {
      emptyReason: "semantic_incomplete",
      semantic: {
        status: "skipped",
        reason: "preparation_budget_exceeded",
        budgetMs: 1000,
      },
    },
  };
  assert.match(
    formatAgentContextResult(budget, {}),
    /exceeded the preparation budget/,
  );
  assert.doesNotMatch(
    formatAgentContextResult(budget, {}),
    /index is not ready/,
  );
});
