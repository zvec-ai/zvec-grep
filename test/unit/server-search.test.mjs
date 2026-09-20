import assert from "node:assert/strict";
import test from "node:test";
import {
  INCOMPATIBLE_SERVER_SEARCH_MESSAGE,
  parseServerSearchResponse,
} from "../../dist/cli/server-search.js";

function response(overrides = {}) {
  return {
    root: "/repo",
    freshness: "fresh",
    result: {
      query: "needle",
      root: "/repo",
      source: "index",
      coverage: "ranked_sample",
      diagnostics: {},
      items: [],
      groupResults: [
        {
          id: "Q1",
          query: "needle",
          role: "primary",
          items: [],
        },
      ],
    },
    ...overrides,
  };
}

test("server search parsing requires grouped structured output", () => {
  assert.equal(
    parseServerSearchResponse(response()).result.groupResults.length,
    1,
  );

  for (const value of [
    {},
    response({ result: { ...response().result, groupResults: undefined } }),
    response({ result: { ...response().result, groupResults: [] } }),
  ]) {
    assert.throws(
      () => parseServerSearchResponse(value),
      (error) => {
        assert.equal(error.message, INCOMPATIBLE_SERVER_SEARCH_MESSAGE);
        assert.equal(error.cause, undefined);
        return true;
      },
    );
  }
});
