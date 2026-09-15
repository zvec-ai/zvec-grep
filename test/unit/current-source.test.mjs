import assert from "node:assert/strict";
import {
  mkdir,
  readdir,
  realpath,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { join, resolve } from "node:path";
import test from "node:test";
import { searchCurrentSource } from "../../dist/search/current-source.js";
import { writeWorkspaceManifest } from "../../dist/engine/manifest.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

test("current-source search uses the supplied absolute root without a service or index", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-current-source-root-");
  const path = join(root, "answer.ts");
  await writeFile(
    path,
    "export function ExplicitRootMarker() { return 42; }\n",
  );

  const searched = await searchCurrentSource({
    root,
    query: "ExplicitRootMarker",
    options: { root: resolve("test/fixtures/repository") },
  });

  assert.equal(searched.kind, "text");
  assert.equal(searched.result.root, root);
  assert.equal(searched.result.source, "rg");
  assert.equal(searched.result.items.length, 1);
  assert.equal(searched.result.items[0].file.absolutePath, path);
  assert.equal(
    searched.result.items[0].metadata.symbolName,
    "ExplicitRootMarker",
  );
  assert.equal(searched.result.items[0].status, "fresh");
  assert.equal(searched.result.workspaceIndex, undefined);
  assert.equal(searched.result.diagnostics.index, undefined);
  assert.equal(searched.result.diagnostics.keywords, undefined);
  assert.deepEqual(await readdir(root), ["answer.ts"]);
});

test("current-source keyword retrieval is opt-in and returns bounded current source evidence", async (t) => {
  const root = await createTemporaryDirectory(
    t,
    "zvec-current-source-keyword-",
  );
  await writeFile(
    join(root, "auth.ts"),
    "// permission validation SHARED_KEYWORD_MARKER\nexport const enabled = true;\n",
  );
  const input = {
    root,
    query: "where does permission validation occur",
    options: {},
  };
  const literal = await searchCurrentSource(input);
  assert.equal(literal.kind, "text");
  assert.deepEqual(literal.result.items, []);
  assert.equal(literal.result.diagnostics.keywords, undefined);

  const keyword = await searchCurrentSource({
    ...input,
    includeKeywords: true,
  });
  assert.equal(keyword.kind, "text");
  assert.equal(keyword.result.source, "rg");
  assert.equal(keyword.result.coverage, "ranked_sample");
  assert.equal(keyword.result.items.length, 1);
  assert.equal(keyword.result.items[0].matchedBy, "keyword");
  assert.equal(keyword.result.items[0].status, "fresh");
  assert.match(keyword.result.items[0].content, /SHARED_KEYWORD_MARKER/);
  assert.ok(keyword.result.diagnostics.keywords.candidates <= 200);
  assert.equal(keyword.result.diagnostics.keywords.truncated, false);
  await assert.rejects(stat(join(root, ".zvec-grep")), { code: "ENOENT" });
});

test("current-source search snapshots caller options and query before its first await", async (t) => {
  const root = await createTemporaryDirectory(
    t,
    "zvec-current-source-snapshot-",
  );
  await writeFile(
    join(root, "keep.ts"),
    "// permission validation ORIGINAL_FILTER\n",
  );
  await writeFile(
    join(root, "other.md"),
    "permission validation CHANGED_FILTER\n",
  );
  const options = { globs: ["*.ts"], fileTypes: ["ts"], limit: 5 };
  const input = {
    root,
    query: "where does permission validation occur",
    options,
    includeKeywords: true,
  };
  const pending = searchCurrentSource(input);
  options.globs.splice(0, 1, "*.md");
  options.fileTypes.splice(0, 1, "md");
  options.limit = 0;
  input.query = "not the original request";
  input.root = resolve("test/fixtures/repository");
  input.includeKeywords = false;

  const result = await pending;
  assert.equal(result.kind, "text");
  assert.equal(result.result.root, root);
  assert.equal(result.result.query, "where does permission validation occur");
  assert.deepEqual(
    result.result.items.map((item) => item.file.relativePath),
    ["keep.ts"],
  );
  assert.match(result.result.items[0].content, /ORIGINAL_FILTER/);
  assert.equal(result.result.items[0].matchedBy, "keyword");
});

test("current-source searches manifest scopes and then applies caller filters from a subdirectory", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-current-source-scopes-");
  const source = join(root, "src");
  const docs = join(root, "docs");
  await mkdir(source);
  await mkdir(docs);
  for (const path of [
    join(source, "keep.ts"),
    join(source, "skip.ts"),
    join(docs, "guide.md"),
    join(root, "outside.ts"),
  ]) {
    await writeFile(path, "// permission validation SCOPED_MARKER\n");
  }
  writeManifest(root, [
    { absolutePath: source, recursive: true, globs: ["*.ts", "!skip.ts"] },
    { absolutePath: docs, recursive: true },
  ]);
  const searched = await searchCurrentSource({
    root: source,
    query: "where does permission validation occur",
    options: { globs: ["src/**"] },
    includeKeywords: true,
  });

  assert.equal(searched.kind, "text");
  assert.equal(searched.result.root, await realpath(root));
  assert.deepEqual(
    searched.result.items.map((item) => item.file.relativePath),
    ["src/keep.ts"],
  );
  assert.equal(searched.result.items[0].matchedBy, "keyword");
  assert.deepEqual(await readdir(join(root, ".zvec-grep")), ["manifest.json"]);
});

test(
  "current-source preserves canonical display paths for configured directory aliases",
  { skip: process.platform === "win32" },
  async (t) => {
    const root = await createTemporaryDirectory(
      t,
      "zvec-current-source-alias-",
    );
    const source = join(root, "src");
    const alias = join(root, "source-alias");
    await mkdir(source);
    await writeFile(
      join(source, "auth.ts"),
      "// permission validation ALIAS_MARKER\n",
    );
    await symlink(source, alias, "dir");
    writeManifest(root, [{ absolutePath: alias, recursive: true }]);

    const result = await searchCurrentSource({
      root,
      query: "where does permission validation occur",
      options: { globs: ["src/**"] },
      includeKeywords: true,
    });
    assert.equal(result.kind, "text");
    assert.deepEqual(
      result.result.items.map((item) => item.file.relativePath),
      ["src/auth.ts"],
    );
    assert.match(result.result.items[0].content, /ALIAS_MARKER/);
  },
);

test("current-source file-only lookup requires explicit opt-in and text callers keep snippets", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-current-source-path-");
  await mkdir(join(root, "src"));
  await writeFile(
    join(root, "src", "auth.ts"),
    "export const enabled = true;\n",
  );
  await writeFile(join(root, "notes.md"), "src/auth.ts owns authentication\n");
  const input = { root, query: "src/auth.ts", options: {} };

  const paths = await searchCurrentSource({ ...input, allowFileLookup: true });
  assert.deepEqual(paths, {
    kind: "files",
    paths: ["src/auth.ts"],
    truncated: false,
  });
  const text = await searchCurrentSource(input);
  assert.equal(text.kind, "text");
  assert.equal(text.result.items.length, 1);
  assert.equal(text.result.items[0].file.relativePath, "notes.md");
  assert.match(text.result.items[0].content, /owns authentication/);
});

test("current-source validation and cancellation do not start discovery or rewrite abort reasons", async () => {
  await assert.rejects(
    searchCurrentSource({ root: "relative", query: "value", options: {} }),
    /absolute root/,
  );
  await assert.rejects(
    searchCurrentSource({ root: resolve("."), query: "  ", options: {} }),
    /non-empty query/,
  );
  const reason = { code: "caller-cancelled" };
  const signal = AbortSignal.abort(reason);
  await assert.rejects(
    searchCurrentSource({ root: "invalid", query: "", options: {}, signal }),
    (error) => error === reason,
  );
});

function writeManifest(root, rootPaths) {
  writeWorkspaceManifest(join(root, ".zvec-grep"), {
    manifestVersion: 1,
    id: "current-source-fixture",
    name: "current-source-fixture",
    path: root,
    rootPaths,
    indexPolicy: "enabled",
    embedding: null,
    indexVersion: null,
    embeddingRuntime: {},
    createdTime: 1,
    updatedTime: 1,
  });
}
