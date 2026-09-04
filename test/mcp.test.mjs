import assert from "node:assert/strict";
import test from "node:test";
import { parseArgs } from "../dist/cli/args.js";
import {
  MCP_MAX_QUERY_GROUPS,
  zvecGrepCliSearchInputSchema,
  zvecGrepIndexInputSchema,
  zvecGrepSearchInputSchema,
} from "../dist/mcp/schemas.js";

test("stdio MCP entry points are not public CLI commands", () => {
  assert.throws(() => parseArgs(["serve", "--mcp"]), /Unknown option/i);
  assert.throws(() => parseArgs(["--mcp"]), /Unknown option/i);
});

test("public MCP search omits runtime overrides while CLI admin preserves them", () => {
  const searchRuntime = {
    apiKey: "request-key",
    device: "auto",
  };
  const indexRuntime = {
    ...searchRuntime,
    endpoint: "https://example.test/embeddings",
  };
  assert.deepEqual(
    zvecGrepIndexInputSchema.parse({ root: "/repo", ...indexRuntime }),
    { root: "/repo", ...indexRuntime },
  );
  assert.deepEqual(
    zvecGrepSearchInputSchema.parse({
      root: "/repo",
      query: "needle",
      ...searchRuntime,
    }),
    {
      root: "/repo",
      query: "needle",
      symbolTypes: [],
      freshness: "eventual",
      autoUpdate: true,
    },
  );
  assert.equal("apiKey" in zvecGrepSearchInputSchema.shape, false);
  assert.equal("device" in zvecGrepSearchInputSchema.shape, false);
  assert.equal("endpoint" in zvecGrepSearchInputSchema.shape, false);
  assert.deepEqual(
    zvecGrepCliSearchInputSchema.parse({
      root: "/repo",
      query: "needle",
      ...searchRuntime,
    }),
    {
      root: "/repo",
      query: "needle",
      ...searchRuntime,
      symbolTypes: [],
      freshness: "eventual",
      autoUpdate: true,
    },
  );
});

test("internal CLI search accepts the legacy combined supplemental-route bound", () => {
  const routes = Array.from(
    { length: MCP_MAX_QUERY_GROUPS * 2 },
    (_, index) => ({
      mode: index % 2 === 0 ? "fts" : "vector",
      query: `route-${index}`,
    }),
  );
  assert.equal(
    zvecGrepCliSearchInputSchema.parse({ root: "/repo", routes }).routes.length,
    MCP_MAX_QUERY_GROUPS * 2,
  );
  assert.throws(
    () =>
      zvecGrepCliSearchInputSchema.parse({
        root: "/repo",
        routes: [...routes, { mode: "fts", query: "too-many" }],
      }),
    /too_big|too big|array/i,
  );
});
