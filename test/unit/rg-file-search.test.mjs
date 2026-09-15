import assert from "node:assert/strict";
import { mkdir, symlink, utimes, writeFile } from "node:fs/promises";
import { join, relative } from "node:path";
import test from "node:test";
import { runRgFileSearch } from "../../dist/engine/service/lexical.js";
import { livePathPriority } from "../../dist/cli/live-search.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

test("file discovery finds names absent from contents and prioritizes exact paths before limiting", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-file-names-");
  await mkdir(join(root, "a"));
  await mkdir(join(root, "src"));
  await mkdir(join(root, "a", "src"));
  for (const path of ["src/auth.ts", "a/src/auth.ts", "a/auth.ts"]) {
    await writeFile(join(root, path), "// nothing related to the filename\n");
  }
  const lookup = (query, extra = {}) =>
    runRgFileSearch({
      root,
      rankPath: (path) => livePathPriority(query, relative(root, path), path),
      ...extra,
    });
  const result = await lookup("src/auth.ts", { limit: 1 });
  assert.deepEqual(result.paths, [join(root, "src/auth.ts")]);
  assert.equal(result.truncated, true);
  assert.equal((await lookup("auth.ts")).paths.length, 3);
  assert.deepEqual((await lookup("missing.ts")).paths, []);
  assert.deepEqual(
    (await lookup("auth.ts", { paths: [join(root, "missing")] })).paths,
    [],
  );
  assert.deepEqual((await lookup("auth.ts", { globs: ["src/**"] })).paths, [
    join(root, "src/auth.ts"),
  ]);
  assert.deepEqual(
    (await lookup("auth.ts", { excludedFileTypes: ["ts"] })).paths,
    [],
  );
  assert.deepEqual(
    (await lookup("auth.ts", { maxFileSizeBytes: 1 })).paths,
    [],
  );
  await utimes(join(root, "src/auth.ts"), 1, 1);
  assert.deepEqual(
    (await lookup("src/auth.ts", { modifiedBefore: 2_000 })).paths,
    [join(root, "src/auth.ts")],
  );
});

test("file discovery honors hidden, ignore, follow and root boundaries, including newlines in names", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-file-scope-");
  const outside = await createTemporaryDirectory(t, "zvec-file-outside-");
  for (const name of [".hidden", ".git", ".zvec-grep", "src"])
    await mkdir(join(root, name));
  for (const name of [
    ".hidden/auth.ts",
    ".git/auth.ts",
    ".zvec-grep/auth.ts",
    "src/auth.ts",
    "ignored.ts",
    "new\nline.ts",
  ])
    await writeFile(join(root, name), "content\n");
  await writeFile(join(root, ".ignore"), "ignored.ts\n");
  await writeFile(join(outside, "auth.ts"), "outside\n");
  await symlink(outside, join(root, "alias"), "dir");
  const list = async (extra = {}) =>
    (await runRgFileSearch({ root, rankPath: () => 1, ...extra })).paths.map(
      (path) => relative(root, path),
    );
  assert.deepEqual((await list()).sort(), ["new\nline.ts", "src/auth.ts"]);
  const hidden = await list({ hidden: true, noIgnore: true, follow: true });
  for (const name of [
    ".hidden/auth.ts",
    "ignored.ts",
    "alias/auth.ts",
    "new\nline.ts",
  ])
    assert.ok(hidden.includes(name), name);
  assert.ok(
    hidden.every(
      (name) => !name.startsWith(".git/") && !name.startsWith(".zvec-grep/"),
    ),
  );
  assert.deepEqual(await list({ paths: [join(root, "src")] }), ["src/auth.ts"]);
});
