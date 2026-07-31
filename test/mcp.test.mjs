import assert from "node:assert/strict";
import test from "node:test";
import { parseArgs } from "../dist/cli/args.js";
import {
  zvecGrepIndexInputSchema,
  zvecGrepSearchInputSchema,
} from "../dist/mcp/schemas.js";

test("stdio MCP entry points are not public CLI commands", () => {
  assert.throws(() => parseArgs(["serve", "--mcp"]), /removed/i);
  assert.throws(() => parseArgs(["--mcp"]), /Unknown command/i);
});

test("MCP index and search expose only supported runtime overrides", () => {
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
    zvecGrepSearchInputSchema.parse({ root: "/repo", ...searchRuntime }),
    {
      root: "/repo",
      ...searchRuntime,
      symbolTypes: [],
      freshness: "eventual",
      autoUpdate: true,
    },
  );
  assert.equal("endpoint" in zvecGrepSearchInputSchema.shape, false);
});
