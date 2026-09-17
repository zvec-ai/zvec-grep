import assert from "node:assert/strict";
import test from "node:test";
import {
  hasStrongLiveMatch,
  hasCompleteLiveLookup,
  liveSearchOptions,
  rankLiveResults,
  LIVE_CANDIDATE_LIMIT,
} from "../../dist/cli/live-search.js";

const item = (rank, symbolName, id = symbolName) => ({
  rank,
  file: { absolutePath: `/repo/${id}.ts` },
  range: { startLine: rank },
  metadata: { kind: "code", symbolName },
  container: { entityId: id },
});

test("complete code-shaped lookups can truthfully return no matches or references", () => {
  for (const query of ["missingHandler", "CONNECTION_POOL", "sha256"]) {
    assert.equal(
      hasCompleteLiveLookup({ query, coverage: "rg_exhaustive", items: [] }),
      true,
    );
    assert.equal(
      hasCompleteLiveLookup({ query, coverage: "rg_truncated", items: [] }),
      false,
    );
  }
  for (const query of ["Authentication", "cache", "cache memory", "$foo"]) {
    assert.equal(
      hasCompleteLiveLookup({ query, coverage: "rg_exhaustive", items: [] }),
      false,
    );
  }
});

test("live search uses literal smart-case matching and keeps selection options", () => {
  const options = liveSearchOptions("AuthService", {
    globs: ["src/**"],
    fileTypes: ["ts"],
    limit: 1,
  });
  assert.equal(options.rg, true);
  assert.equal(options.autoUpdate, false);
  assert.equal(options.rgOptions.fixedStrings, true);
  assert.equal(options.rgOptions.wordRegexp, true);
  assert.equal(options.rgOptions.ignoreCase, false);
  assert.equal(options.limit, LIVE_CANDIDATE_LIMIT);
  assert.deepEqual(options.globs, ["src/**"]);
  assert.deepEqual(options.fileTypes, ["ts"]);
  assert.equal(
    liveSearchOptions("connection pool", {}).rgOptions.ignoreCase,
    true,
  );
  assert.equal(liveSearchOptions("call(x)", {}).rgOptions.fixedStrings, true);
});

test("live definitions precede references, deduplicate, and honor the final limit", () => {
  const result = {
    query: "AuthService",
    coverage: "rg_exhaustive",
    items: [item(1, "caller"), item(2, "AuthService"), item(3, "AuthService")],
  };
  const ranked = rankLiveResults(result, 1);
  assert.equal(ranked.items.length, 1);
  assert.equal(ranked.items[0].metadata.symbolName, "AuthService");
  assert.equal(ranked.items[0].rank, 1);
  assert.equal(ranked.coverage, "rg_truncated");
  assert.equal(hasStrongLiveMatch(ranked), true);
  for (const query of ["cache", "AuthService token refresh", "索引在哪里"]) {
    assert.equal(hasStrongLiveMatch({ query, items: [item(1, query)] }), false);
  }
  assert.equal(
    hasStrongLiveMatch({
      query: "AuthService",
      items: [item(1, "MockAuthService")],
    }),
    false,
  );
});
