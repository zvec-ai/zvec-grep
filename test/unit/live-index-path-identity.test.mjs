import assert from "node:assert/strict";
import { mkdir, realpath, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import {
  liveIndexPathIdentities,
  mergeLiveAndIndexedResults,
} from "../../dist/cli/live-index-merge.js";
import { printCliContextResult } from "../../dist/cli/format/context.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

const range = (line) => ({
  kind: "text",
  startLine: line,
  endLine: line,
  startOffset: 0,
  endOffset: 50,
});
const metadata = (name) => ({
  kind: "code",
  symbolType: "function",
  symbolName: name,
  scope: null,
  nodeType: "function_declaration",
  signature: `function ${name}()`,
  doc: null,
  modifiers: [],
});
function item(kind, absolutePath, relativePath, rank, line, name, id) {
  const base = {
    kind,
    rank,
    file: { absolutePath, relativePath },
    range: range(line),
    content: `function ${name}() { return "connection pool"; }`,
    contentRole: "source",
    metadata: metadata(name),
    status: "fresh",
    matchedBy: kind === "lexical_match" ? "lexical" : "vector",
  };
  return kind === "lexical_match"
    ? {
        ...base,
        container: {
          entityId: id,
          range: range(line),
          metadata: metadata(name),
        },
      }
    : { ...base, entityId: id };
}
function result(source, items) {
  return {
    source,
    query: "connection pool",
    root: "/repo",
    items,
    coverage: source === "rg" ? "rg_exhaustive" : "ranked_sample",
    diagnostics:
      source === "rg"
        ? {}
        : { index: { hitsReturned: items.length, routes: [] } },
  };
}

test("real path aliases deduplicate the same symbol, retain other files and share one displayed file group", async (t) => {
  const temporary = await createTemporaryDirectory(t, "zvec-merge-path-");
  const physical = join(temporary, "physical");
  const alias = join(temporary, "alias");
  const other = join(temporary, "other");
  await mkdir(physical);
  await mkdir(other);
  await writeFile(join(physical, "pool.ts"), "source fixture\n");
  await writeFile(join(other, "pool.ts"), "different physical file\n");
  await symlink(
    physical,
    alias,
    process.platform === "win32" ? "junction" : "dir",
  );
  const canonicalFile = await realpath(join(physical, "pool.ts"));
  const aliasFile = join(alias, "pool.ts");
  const otherFile = join(other, "pool.ts");
  const live = result("rg", [
    item(
      "lexical_match",
      canonicalFile,
      "src/pool.ts",
      1,
      2,
      "openPool",
      "live:physical",
    ),
    // Different extraction IDs can arise from two configured path aliases.
    item(
      "lexical_match",
      aliasFile,
      "alias/pool.ts",
      2,
      2,
      "openPool",
      "live:alias",
    ),
  ]);
  const indexed = result("index", [
    item(
      "indexed_entity",
      aliasFile,
      "legacy-alias/pool.ts",
      1,
      2,
      "openPool",
      "index:open",
    ),
    item(
      "indexed_entity",
      aliasFile,
      "legacy-alias/pool.ts",
      2,
      8,
      "closePool",
      "index:close",
    ),
    item(
      "indexed_entity",
      otherFile,
      "other/pool.ts",
      3,
      2,
      "openPool",
      "index:other",
    ),
  ]);
  // Daemon responses may expose candidates solely in the CLI's group list.
  indexed.groupResults = [
    {
      id: "Q1",
      query: indexed.query,
      role: "primary",
      items: indexed.items,
    },
  ];
  indexed.items = [];
  const before = structuredClone({ indexed, live });
  const resolved = [];
  const identities = await liveIndexPathIdentities(
    indexed,
    live,
    async (path) => {
      resolved.push(path);
      return realpath(path);
    },
  );
  const merged = mergeLiveAndIndexedResults(
    indexed,
    live,
    10,
    identities,
  ).result;
  assert.deepEqual(
    merged.items.map((hit) => hit.metadata.symbolName),
    ["openPool", "closePool", "openPool"],
  );
  assert.deepEqual(
    merged.items.map((hit) => hit.file.relativePath),
    ["src/pool.ts", "src/pool.ts", "other/pool.ts"],
  );
  assert.equal(merged.items[0].kind, "lexical_match");
  assert.equal(merged.items[0].file.absolutePath, canonicalFile);
  assert.equal(merged.items[1].file.absolutePath, canonicalFile);
  assert.equal(merged.items[2].file.absolutePath, otherFile);
  assert.equal(merged.diagnostics.index.hitsReturned, 2);
  assert.equal(
    resolved.length,
    3,
    "resolve each distinct returned path only once",
  );
  assert.equal(new Set(resolved).size, 3);
  assert.equal(identities.get(aliasFile), identities.get(canonicalFile));
  assert.notEqual(identities.get(otherFile), identities.get(canonicalFile));
  assert.deepEqual(
    { indexed, live },
    before,
    "preserve the input display paths and snapshots",
  );
  const lines = [];
  const previous = console.log;
  try {
    console.log = (...args) => lines.push(args.join(" "));
    printCliContextResult(merged, { human: true, color: "never" });
  } finally {
    console.log = previous;
  }
  const output = lines.join("\n");
  assert.equal((output.match(/^File\s*:\s*src\/pool\.ts$/gm) ?? []).length, 1);
  assert.doesNotMatch(output, /legacy-alias\/pool\.ts|alias\/pool\.ts/);
});

test("missing path identity falls back safely and explicit routes do no filesystem resolution", async () => {
  const live = result("rg", [
    item("lexical_match", "/repo/a.ts", "a.ts", 1, 1, "openPool", "live:a"),
  ]);
  const indexed = result("index", [
    item("indexed_entity", "/repo/b.ts", "b.ts", 1, 1, "openPool", "index:b"),
  ]);
  let calls = 0;
  const unavailable = async () => {
    calls++;
    throw new Error("path disappeared");
  };
  const identities = await liveIndexPathIdentities(indexed, live, unavailable);
  assert.equal(calls, 2);
  assert.equal(identities.get("/repo/a.ts"), "/repo/a.ts");
  assert.equal(identities.get("/repo/b.ts"), "/repo/b.ts");
  assert.equal(
    mergeLiveAndIndexedResults(indexed, live, 10, identities).result.items
      .length,
    2,
  );
  calls = 0;
  assert.equal(
    (await liveIndexPathIdentities(indexed, undefined, unavailable)).size,
    0,
  );
  assert.equal(calls, 0);
});
