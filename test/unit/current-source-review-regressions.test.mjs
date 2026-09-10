import assert from "node:assert/strict";
import childProcess from "node:child_process";
import { mkdir, mkdtemp, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { syncBuiltinESMExports } from "node:module";
import { join, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

// Allow the reviewer to run these exact tests against the immutable pre-fix
// build without copying or rebuilding the original checkout.
const dist = process.env.ZVEC_TEST_DIST_ROOT
  ? resolve(process.env.ZVEC_TEST_DIST_ROOT)
  : fileURLToPath(new URL("../../dist/", import.meta.url));
const { searchCurrentSource } = await import(
  pathToFileURL(join(dist, "search/current-source.js")).href
);
const { runRgSearch } = await import(
  pathToFileURL(join(dist, "engine/service/lexical.js")).href
);
const { enrichLexicalItemsWithStructure } = await import(
  pathToFileURL(join(dist, "engine/service/structure-enrichment.js")).href
);
const { writeWorkspaceManifest } = await import(
  pathToFileURL(join(dist, "engine/manifest.js")).href
);

for (const [label, selection] of [
  ["glob", { globs: ["target.ts"] }],
  ["file type", { fileTypes: ["ts"] }],
]) {
  test(`current-source applies caller ${label} selection before the manifest literal candidate cap`, async (t) => {
    const { root, temporary } = await fixture(t);
    const query = "needleToken";
    await writeFile(
      join(root, "a-noise.md"),
      Array.from({ length: 201 }, (_, i) => `${query} unrelated ${i}\n`).join(
        "",
      ),
    );
    await writeFile(
      join(root, "target.ts"),
      `export function ${query}() { return "SELECTED_TARGET_MARKER"; }\n`,
    );
    await writeFile(
      join(root, "excluded.ts"),
      `export const excluded = "${query} MANIFEST_EXCLUDED_MARKER";\n`,
    );
    // Native rg sorts paths serially, so the 201 disallowed hits provably
    // precede target.ts. This is a fixture-only config, not a timing race.
    const config = join(temporary, "rg-config");
    await writeFile(config, "--sort\npath\n");
    const previousConfig = process.env.RIPGREP_CONFIG_PATH;
    process.env.RIPGREP_CONFIG_PATH = config;
    t.after(() => {
      if (previousConfig === undefined) delete process.env.RIPGREP_CONFIG_PATH;
      else process.env.RIPGREP_CONFIG_PATH = previousConfig;
    });
    const beforeFilter = await runRgSearch({
      root,
      patterns: [query],
      limit: 200,
    });
    assert.equal(beforeFilter.items.length, 200);
    assert.equal(beforeFilter.diagnostics.truncated, true);
    assert.ok(
      beforeFilter.items.every(
        (item) => item.file.relativePath === "a-noise.md",
      ),
      "the fixture must exercise 200 retained off-scope matches before the target",
    );

    const withoutManifest = await searchCurrentSource({
      root,
      query,
      options: { globs: ["target.ts"] },
    });
    assert.deepEqual(paths(withoutManifest), ["target.ts"]);
    await assert.rejects(stat(join(root, ".zvec-grep")), { code: "ENOENT" });

    writeManifest(root, [
      { absolutePath: root, recursive: true, globs: ["!excluded.ts"] },
    ]);
    const result = await searchCurrentSource({
      root,
      query,
      options: selection,
    });
    assert.deepEqual(paths(result), ["target.ts"], JSON.stringify(result));
    assert.match(result.result.items[0].content, /SELECTED_TARGET_MARKER/);
    assert.doesNotMatch(
      result.result.items.map((item) => item.content).join("\n"),
      /unrelated|MANIFEST_EXCLUDED_MARKER/,
    );
    assert.equal(result.result.items[0].matchedBy, "lexical");
    assert.equal(result.result.items[0].status, "fresh");
    assert.equal(result.result.diagnostics.keywords, undefined);
  });
}

test("ordinary literal lookup identifies each same-line function without clipping its source window", async (t) => {
  const { root } = await fixture(t);
  const names = ["alphaLookup", "betaLookup"];
  const source =
    "export function alphaLookup() { return 1; } export function betaLookup() { return 2; }";
  await writeFile(join(root, "source.ts"), `${source}\n`);
  const results = [];
  for (const query of names) {
    const result = await searchCurrentSource({ root, query, options: {} });
    assert.equal(result.kind, "text");
    assert.equal(result.result.items.length, 1);
    const item = result.result.items[0];
    assert.equal(
      item.content,
      source,
      "literal context remains the original full source line",
    );
    assert.equal(item.excerptRange.startOffset, source.indexOf(query));
    assert.equal(item.status, "fresh");
    assert.equal(item.matchedBy, "lexical");
    assert.equal(result.result.diagnostics.keywords, undefined);
    results.push(item);
  }
  assert.deepEqual(
    results.map((item) => item.container?.metadata?.symbolName),
    names,
    "each literal match must select its own containing source fragment, not the same line-only owner",
  );
  assert.deepEqual(
    results.map((item) => item.metadata?.symbolName),
    names,
  );

  // The new default caller must not change legacy managed-rg enrichment or
  // opt it into the strict keyword path's clipped source presentation.
  const legacy = await runRgSearch({ root, patterns: [names[1]] });
  const enriched = await enrichLexicalItemsWithStructure(root, legacy.items);
  assert.deepEqual(
    enriched.items.map((item) => item.content),
    legacy.items.map((item) => item.content),
  );
});

for (const [label, extra] of [
  ["literal", {}],
  ["bounded", { scanLimit: 1_000 }],
]) {
  test(`${label} file admission caches exclusions and retains accepted-candidate early stopping`, async (t) => {
    const { root } = await fixture(t);
    await writeFile(join(root, "noise.txt"), "needle noise\n".repeat(201));
    await writeFile(join(root, "target.txt"), "needle target\n".repeat(3));
    const seen = new Map();
    const result = await runRgSearch({
      root,
      patterns: ["needle"],
      limit: 1,
      rgOptions: { extraArgs: ["--sort", "path"] },
      ...extra,
      acceptFile(path) {
        seen.set(path, (seen.get(path) ?? 0) + 1);
        return path.endsWith("target.txt");
      },
    });
    assert.equal(result.items.length, 1);
    assert.equal(result.items[0].file.relativePath, "target.txt");
    assert.equal(result.diagnostics.truncated, true);
    assert.equal(seen.get(join(root, "noise.txt")), 1);
    assert.equal(seen.get(join(root, "target.txt")), 1);
    assert.ok([...seen.values()].every((count) => count === 1));
  });

  test(`${label} file admission failure and cancellation reject after the actual child closes`, async (t) => {
    const { root } = await fixture(t);
    await writeFile(join(root, "source.txt"), "needle\n".repeat(500));
    const spawn = childProcess.spawn;
    const children = [];
    const mock = t.mock.method(childProcess, "spawn", (...args) => {
      const child = spawn(...args);
      const observed = { closed: false };
      children.push(observed);
      child.once("close", () => {
        observed.closed = true;
      });
      return child;
    });
    syncBuiltinESMExports();
    try {
      const reason = new Error("fixture file admission failed");
      await assert.rejects(
        runRgSearch({
          root,
          patterns: ["needle"],
          ...extra,
          acceptFile() {
            throw reason;
          },
        }),
        (error) => error === reason,
      );
      assert.ok(children.length > 0);
      assert.ok(children.every((child) => child.closed));
      const controller = new AbortController();
      const cancellation = { code: "fixture-admission-cancelled" };
      await assert.rejects(
        runRgSearch({
          root,
          patterns: ["needle"],
          ...extra,
          signal: controller.signal,
          acceptFile() {
            controller.abort(cancellation);
            return true;
          },
        }),
        (error) => error === cancellation,
      );
      assert.ok(children.every((child) => child.closed));
      const next = await runRgSearch({ root, patterns: ["needle"], limit: 1 });
      assert.equal(next.items.length, 1);
    } finally {
      mock.mock.restore();
      syncBuiltinESMExports();
    }
  });
}

async function fixture(t) {
  const temporary = await mkdtemp(join(tmpdir(), "zvec-current-review-"));
  const root = join(temporary, "repo");
  await mkdir(root);
  t.after(() => rm(temporary, { recursive: true, force: true }));
  return { temporary, root };
}

function paths(result) {
  assert.equal(result.kind, "text");
  return [
    ...new Set(result.result.items.map((item) => item.file.relativePath)),
  ].sort();
}

function writeManifest(root, rootPaths) {
  writeWorkspaceManifest(join(root, ".zvec-grep"), {
    manifestVersion: 1,
    id: "current-source-review-fixture",
    name: "current-source-review-fixture",
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
