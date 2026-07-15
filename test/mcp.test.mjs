import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { createFakeEmbeddingServer } from "./helpers/fake-embedding.mjs";
import { runCli } from "./helpers/fixtures.mjs";

const cliPath = resolve("dist/cli/index.js");

test("MCP exposes and executes indexed and no-index search tools", async (t) => {
  const temporaryDirectory = await mkdtemp(join(tmpdir(), "zvec-grep-mcp-"));
  const root = join(temporaryDirectory, "repo");
  const home = join(temporaryDirectory, "home");
  await mkdir(root, { recursive: true });
  await writeFile(
    join(root, "sample.ts"),
    "export const McpIndexedSymbol = 42;\n",
  );
  const endpoint = await createFakeEmbeddingServer(t);
  const env = {
    ...process.env,
    HOME: home,
    NO_COLOR: "1",
    ZVEC_GREP_API_KEY: "test-key",
    ZVEC_GREP_EMBEDDING: "qwen/text-embedding-v4",
    ZVEC_GREP_ENDPOINT: endpoint,
    ZVEC_GREP_HOME: home,
  };
  await runCli(
    [
      "--index",
      "--embedding",
      "qwen/text-embedding-v4",
      "--api-key",
      "test-key",
      "--endpoint",
      endpoint,
      root,
    ],
    { cwd: root, env, timeout: 120_000 },
  );
  const client = new Client({ name: "zvec-grep-test", version: "1.0.0" });
  const transport = new StdioClientTransport({
    command: process.execPath,
    args: [cliPath, "serve", "--mcp"],
    cwd: root,
    env,
    stderr: "pipe",
  });
  t.after(async () => {
    await client.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await client.connect(transport);
  const listed = await client.listTools();
  const names = listed.tools.map((tool) => tool.name).sort();

  assert.deepEqual(names, ["zvec_grep_rg", "zvec_grep_search"]);
  assert.equal(names.includes("zvec_grep_index"), false);
  assert.equal(names.includes("zvec_grep_status"), false);

  const lexical = await client.callTool({
    name: "zvec_grep_rg",
    arguments: {
      root,
      pattern: "McpIndexedSymbol",
      fixedStrings: true,
    },
  });
  assert.equal(lexical.isError, undefined);
  assert.match(
    lexical.content.map((item) => item.text ?? "").join("\n"),
    /McpIndexedSymbol/,
  );

  const indexed = await client.callTool({
    name: "zvec_grep_search",
    arguments: {
      root,
      fts: "McpIndexedSymbol",
      limit: 5,
    },
  });
  assert.equal(indexed.isError, undefined);
  assert.match(
    indexed.content.map((item) => item.text ?? "").join("\n"),
    /McpIndexedSymbol/,
  );

  const invalid = await client.callTool({
    name: "zvec_grep_search",
    arguments: { root },
  });
  assert.equal(invalid.isError, true);
  assert.match(
    invalid.content.map((item) => item.text ?? "").join("\n"),
    /requires query, queries, fts, or vector/,
  );
});
