import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { inWorker } from "../helpers/glob-worker.mjs";
import * as glob from "../../dist/engine/utils/glob.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

test("glob matching preserves wildcard, directory, class, and brace semantics", () => {
  const cases = [
    ["*.ts", "src/main.ts", true],
    ["*.ts", "src/main.js", false],
    ["src/*.ts", "src/nested/main.ts", false],
    ["src/**/main.ts", "src/main.ts", true],
    ["src/**/main.ts", "src/a/b/main.ts", true],
    ["src/**", "src", true],
    ["src/**", "src/a/b", true],
    ["*.{js,ts}", "src/main.ts", true],
    ["{a,{b,c}}.ts", "c.ts", true],
    ["{,a}b", "b", true],
    ["{a,}b", "ab", true],
    ["file[0-9].ts", "file2.ts", true],
    ["file[!0-9].ts", "filex.ts", true],
    ["file[^0-9].ts", "file2.ts", false],
    ["[[]", "[", true],
    ["a[", "a[", true],
    ["a{b}", "a{b}", true],
    ["a[]", "a[]", true],
    ["a[!]", "a[!]", true],
    ["a[^]", "a[^]", true],
    ["??", "😀", true],
    ["?", "😀", false],
    ["**Z", "a\nZ", false],
    ["*Z", "a\nZ", true],
  ];
  for (const [pattern, path, expected] of cases) {
    assert.equal(
      glob.ripgrepGlobMatches(pattern, path),
      expected,
      JSON.stringify([pattern, path]),
    );
  }
  assert.throws(() => glob.ripgrepGlobMatches("[z-a]", "a"), SyntaxError);
  assert.equal(glob.ripgrepGlobMatches("", "a"), false);
});

test("glob helpers retain literal-path and case normalization behavior", () => {
  assert.equal(glob.pathPatternMatches("src", "src/main.ts"), true);
  assert.equal(glob.ripgrepGlobMatches("src", "src/main.ts"), false);
  assert.equal(glob.pathPatternMatches("{a,b}", "a"), false);
  assert.equal(glob.pathPatternMatches(" ./src//*.ts ", "src\\main.ts"), true);
  assert.equal(glob.pathPatternMatches("/src/*.ts", "/src/main.ts"), true);
  assert.equal(
    glob.pathPatternMatches("C:\\src\\*.ts", "C:/src/main.ts"),
    true,
  );
  assert.equal(glob.pathPatternMatches("", "src"), false);
  assert.equal(
    glob.pathPatternMatchesCaseInsensitive("SRC", "src/main.ts"),
    true,
  );
  assert.equal(
    glob.pathPatternMatchesCaseInsensitive("*.TS", "src/main.ts"),
    true,
  );
  assert.equal(
    glob.ripgrepGlobMatchesCaseInsensitive("[A-Z]*.TS", "main.ts"),
    true,
  );
  assert.equal(glob.ripgrepGlobMatchesCaseInsensitive("É*", "éclair"), true);
  assert.equal(glob.ripgrepGlobMatchesCaseInsensitive("K*", "Kelvin"), false);
  assert.equal(glob.ripgrepGlobMatchesCaseInsensitive("S*", "ſ"), false);
  assert.equal(glob.ripgrepGlobMatches("*.TS", "main.ts"), false);
  assert.equal(glob.pathPatternMightMatchDescendant("src/*.ts", "src"), true);
  assert.equal(glob.pathPatternMightMatchDescendant("src/**", "docs"), false);
});

test("adversarial wildcard and alternation matching completes without backtracking", async () => {
  await inWorker(`
    for (const pattern of ['**'.repeat(20) + 'Z', '*a'.repeat(20) + 'Z', '**/' + '{a,aa}'.repeat(20) + 'Z']) {
      for (const fn of [glob.pathPatternMatches, glob.pathPatternMatchesCaseInsensitive, glob.ripgrepGlobMatches, glob.ripgrepGlobMatchesCaseInsensitive]) {
        assert.equal(fn(pattern, 'a'.repeat(40)), false);
        assert.equal(fn(pattern, 'a'.repeat(40) + 'Z'), true);
      }
    }
    assert.equal(glob.ripgrepGlobMatches('**'.repeat(20) + 'Z', 'src/authorization/operation.ts'), false);
  `);
});

test("scanner and MCP filters accept the regression pattern without stalling", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-glob-regression-");
  await mkdir(join(root, "src"));
  await writeFile(
    join(root, "src", "operation.ts"),
    "export const answer = 1;\n",
  );
  await writeFile(join(root, ".gitignore"), `${"**".repeat(20)}Z\n`);
  await inWorker(
    `
    const pattern = '**'.repeat(20) + 'Z';
    zvecGrepSearchInputSchema.parse({ root: workerData, fts: 'answer', globs: [pattern] });
    assert.equal((await scanRootPaths('glob-test', [{ absolutePath: workerData, recursive: true }])).files.length, 1);
    assert.equal((await scanRootPaths('glob-test', [{ absolutePath: workerData, recursive: true, globs: [pattern] }])).files.length, 0);
  `,
    root,
  );
});

test("glob complexity limits reject expensive inputs without hanging", async () => {
  await inWorker(
    `
    assert.throws(() => glob.ripgrepGlobMatches('*'.repeat(4097), 'a'), /4096-character/);
    assert.throws(() => glob.ripgrepGlobMatches('*', 'a'.repeat(32769)), /32768-character/);
    assert.throws(() => glob.ripgrepGlobMatches('{a,'.repeat(33) + 'b' + '}'.repeat(33), 'a'), /nesting limit/);
    assert.throws(() => glob.ripgrepGlobMatches('*a'.repeat(1000) + 'Z', 'a'.repeat(4096)), /matching work limit/);
    assert.throws(() => withGlobBudget(() => {
      for (let i = 0; i < 3000; i++) glob.ripgrepGlobMatches('**', 'a'.repeat(8192));
    }), /work limit/);
    // A failed operation must not poison the next one, including cached matchers.
    assert.equal(withGlobBudget(() => glob.ripgrepGlobMatches('**', 'a')), true);
  `,
    undefined,
    10_000,
  );
});

test("invalid and oversized ignore files fail with source context rather than bypassing ignores", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-glob-limits-");
  await writeFile(join(root, "secret.ts"), "export const secret = 1;\n");
  await inWorker(
    `
    const { writeFile } = await import('node:fs/promises');
    const { join } = await import('node:path');
    const ignore = join(workerData, '.gitignore');
    const scan = () => scanRootPaths('glob-limits', [{ absolutePath: workerData, recursive: true }]);
    await writeFile(ignore, '# comment\\n[z-a]\\n');
    await assert.rejects(scan(), /\\.gitignore:2:/);
    await writeFile(ignore, '*'.repeat(4097));
    await assert.rejects(scan(), /\\.gitignore:1:.*4096-character/);
    await writeFile(ignore, '#'.repeat(1048577));
    await assert.rejects(scan(), /1048576-byte.*\\.gitignore/);
    await writeFile(ignore, 'secret.ts\\n'.repeat(10001));
    await assert.rejects(scan(), /10000-rule/);
    await writeFile(ignore, 'secret.ts\\n');
    assert.equal((await scan()).files.length, 0);
    await writeFile(ignore, '');
    assert.equal((await scan()).files.length, 1);
  `,
    root,
  );
});
