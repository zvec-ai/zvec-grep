import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import test from "node:test";
import { canonicalizeGeneratedArtifactMatches } from "../../dist/engine/service/generated-artifacts.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

function match(root, relativePath, rank) {
  return {
    kind: "lexical_match",
    rank,
    file: {
      absolutePath: join(root, ...relativePath.split("/")),
      relativePath,
      rootPath: root,
    },
    range: {
      kind: "text",
      startLine: 1,
      endLine: 1,
      startOffset: 0,
      endOffset: 5,
    },
    content: "match",
    status: "fresh",
    matchedBy: "lexical",
  };
}

async function write(root, relativePath, content) {
  const path = join(root, ...relativePath.split("/"));
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, content);
}

test("canonicalizes an exact build/lib mirror to its source path", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-generated-");
  const contents = "export function answer() { return 42; }\n";
  await write(root, "requests/models.ts", contents);
  await write(root, "build/lib/requests/models.ts", contents);
  const source = match(root, "requests/models.ts", 1);
  const generated = match(root, "build/lib/requests/models.ts", 2);

  const result = await canonicalizeGeneratedArtifactMatches({
    root,
    items: [source, generated],
  });

  assert.deepEqual(
    result.items.map((item) => item.file.relativePath),
    ["requests/models.ts", "requests/models.ts"],
  );
  assert.equal(
    result.items[1].file.absolutePath,
    join(root, "requests", "models.ts"),
  );
  assert.equal(generated.file.relativePath, "build/lib/requests/models.ts");
  assert.deepEqual(result.diagnostics, {
    generatedCandidates: 1,
    generatedMirrorsCanonicalized: 1,
    generatedMatchesDemoted: 0,
  });
});

test("does not return a stripped source path excluded from the rg result scope", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-generated-");
  const contents = "export const apiVersion = 3;\n";
  await write(root, "src/api.ts", contents);
  await write(root, "dist/src/api.ts", contents);

  const result = await canonicalizeGeneratedArtifactMatches({
    root,
    items: [match(root, "dist/src/api.ts", 1)],
  });

  assert.equal(result.items.length, 1);
  assert.equal(result.items[0].file.relativePath, "dist/src/api.ts");
  assert.deepEqual(result.diagnostics, {
    generatedCandidates: 1,
    generatedMirrorsCanonicalized: 0,
    generatedMatchesDemoted: 1,
  });
});

test("does not merge unrelated matched paths merely because their bytes match", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-generated-");
  const contents = "identical bytes under intentionally different names\n";
  await write(root, "src/original.txt", contents);
  await write(root, "generated/renamed.txt", contents);

  const result = await canonicalizeGeneratedArtifactMatches({
    root,
    items: [
      match(root, "generated/renamed.txt", 1),
      match(root, "src/original.txt", 2),
    ],
  });

  assert.deepEqual(
    result.items.map((item) => item.file.relativePath),
    ["src/original.txt", "generated/renamed.txt"],
  );
  assert.deepEqual(
    result.items.map((item) => item.rank),
    [1, 2],
  );
  assert.equal(result.diagnostics.generatedMirrorsCanonicalized, 0);
  assert.equal(result.diagnostics.generatedMatchesDemoted, 1);
});

test("retains non-mirrors and stably demotes them after source matches", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-generated-");
  await write(root, "dist/first.ts", "generated first\n");
  await write(root, "src/source.ts", "ordinary source\n");
  await write(root, "coverage/second.ts", "generated second\n");

  const result = await canonicalizeGeneratedArtifactMatches({
    root,
    items: [
      match(root, "dist/first.ts", 1),
      match(root, "src/source.ts", 2),
      match(root, "coverage/second.ts", 3),
    ],
  });

  assert.deepEqual(
    result.items.map((item) => item.file.relativePath),
    ["src/source.ts", "dist/first.ts", "coverage/second.ts"],
  );
  assert.deepEqual(
    result.items.map((item) => item.rank),
    [1, 2, 3],
  );
  assert.deepEqual(result.diagnostics, {
    generatedCandidates: 2,
    generatedMirrorsCanonicalized: 0,
    generatedMatchesDemoted: 2,
  });
});

test("does not mistake same-sized but different content for a mirror", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-generated-");
  await write(root, "same.ts", "abcde");
  await write(root, "out/same.ts", "edcba");

  const result = await canonicalizeGeneratedArtifactMatches({
    root,
    items: [match(root, "out/same.ts", 1), match(root, "same.ts", 2)],
  });

  assert.deepEqual(
    result.items.map((item) => item.file.relativePath),
    ["same.ts", "out/same.ts"],
  );
  assert.equal(result.diagnostics.generatedMirrorsCanonicalized, 0);
  assert.equal(result.diagnostics.generatedMatchesDemoted, 1);
});

test("returns unique generated-only matches without deleting or reordering them", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-generated-");
  const relativePaths = [
    "build/one.ts",
    "dist/two.ts",
    "out/three.ts",
    "coverage/four.ts",
    ".next/five.ts",
    ".nuxt/six.ts",
    "target/seven.ts",
    "generated/eight.ts",
  ];
  await Promise.all(
    relativePaths.map((relativePath, index) =>
      write(root, relativePath, `unique generated content ${index}\n`),
    ),
  );

  const result = await canonicalizeGeneratedArtifactMatches({
    root,
    items: relativePaths.map((relativePath, index) =>
      match(root, relativePath, index + 1),
    ),
  });

  assert.deepEqual(
    result.items.map((item) => item.file.relativePath),
    relativePaths,
  );
  assert.equal(result.items.length, relativePaths.length);
  assert.deepEqual(result.diagnostics, {
    generatedCandidates: relativePaths.length,
    generatedMirrorsCanonicalized: 0,
    generatedMatchesDemoted: relativePaths.length,
  });
});

test("explicit generated paths and positive globs bypass rewriting and demotion", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-generated-");
  const contents = "an exact mirror\n";
  await write(root, "source.txt", contents);
  await write(root, "build/lib/source.txt", contents);
  const optionCases = [
    { paths: ["build/lib"] },
    { includePaths: ["dist/**"] },
    { globs: ["**/out/**"] },
    { insensitiveGlobs: ["**/TARGET/**"] },
  ];

  for (const filters of optionCases) {
    const result = await canonicalizeGeneratedArtifactMatches({
      root,
      items: [
        match(root, "build/lib/source.txt", 1),
        match(root, "source.txt", 2),
      ],
      ...filters,
    });

    assert.deepEqual(
      result.items.map((item) => item.file.relativePath),
      ["build/lib/source.txt", "source.txt"],
    );
    assert.deepEqual(result.diagnostics, {
      generatedCandidates: 1,
      generatedMirrorsCanonicalized: 0,
      generatedMatchesDemoted: 0,
    });
  }
});

test("a negative generated glob does not disable canonicalization", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-generated-");
  const contents = "mirror excluded only by a negative glob\n";
  await write(root, "source.txt", contents);
  await write(root, "dist/source.txt", contents);

  const result = await canonicalizeGeneratedArtifactMatches({
    root,
    items: [match(root, "dist/source.txt", 1), match(root, "source.txt", 2)],
    globs: ["!**/dist/**"],
  });

  assert.equal(result.items[0].file.relativePath, "source.txt");
  assert.equal(result.items[1].file.relativePath, "source.txt");
  assert.equal(result.diagnostics.generatedMirrorsCanonicalized, 1);
});
