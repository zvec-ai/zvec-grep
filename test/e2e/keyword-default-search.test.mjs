import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdir, rm, stat, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { cliPath, createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { readInstanceRecord } from "../../dist/daemon/server-controller.js";

const exec = promisify(execFile);
const incompleteEmpty = "No text matches; semantic search is not ready.\n";

async function fixture(t) {
  const temporary = await createTemporaryDirectory(t, "zvec-keyword-default-");
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  const modelCache = join(temporary, "models");
  await mkdir(root);
  let requests = 0;
  const server = createServer((_request, response) => {
    requests++;
    response.writeHead(503).end();
  });
  t.after(async () => {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
    assert.equal(requests, 0, "keyword retrieval must not call a server/model");
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  // A custom local URL is deliberately ineligible for implicit daemon startup.
  // Isolate the complete environment, not just HOME: inherited model defaults,
  // credentials, mode, tokens and NODE_OPTIONS must not affect this cold test.
  const env = {
    PATH: process.env.PATH,
    HOME: home,
    USERPROFILE: home,
    ZVEC_GREP_HOME: home,
    ZVEC_GREP_MODEL_CACHE: modelCache,
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${server.address().port}/custom-endpoint`,
    NO_COLOR: "1",
  };
  return {
    root,
    async run(args, cwd = root) {
      const result = await exec(
        process.execPath,
        ["--liftoff-only", cliPath, ...args],
        { cwd, env, timeout: 5_000 },
      );
      assert.equal(requests, 0);
      assert.equal(await readInstanceRecord(home), undefined);
      for (const path of [
        join(root, ".zvec-grep"),
        join(cwd, ".zvec-grep"),
        modelCache,
      ]) {
        await assert.rejects(stat(path), { code: "ENOENT" });
      }
      return result;
    },
  };
}

function assertKeywordResult(result) {
  assert.match(result.stdout, /matchedBy=keyword\b/);
  assert.doesNotMatch(result.stdout, /No (?:text )?matches/);
  assert.match(result.stderr, /text search only/);
  assert.match(result.stderr, /semantic index is not ready/);
}

test("cold keyword search displays an unowned comment once despite multiple term anchors", async (t) => {
  const state = await fixture(t);
  await writeFile(
    join(state.root, "auth.ts"),
    "// permission validation\nexport function verifyToken(token) { return Boolean(token); }\n",
  );
  const result = await state.run(["where does permission validation occur"]);
  assertKeywordResult(result);
  assert.equal(
    result.stdout,
    "auth.ts\n  matchedBy=keyword\n  1:\t// permission validation\n",
  );
});

test("cold default keyword search finds a complete code-subword match after many weak same-file matches", async (t) => {
  const state = await fixture(t);
  await writeFile(
    join(state.root, "pool.ts"),
    [
      ...Array.from(
        { length: 240 },
        (_, index) => `export const connectionHint${index} = ${index};`,
      ),
      "",
      "export function connectionPool() {",
      '  return "COMPLETE_KEYWORD_EVIDENCE";',
      "}",
      "",
    ].join("\n"),
  );
  await writeFile(
    join(state.root, "weak.ts"),
    'export function poolOnly() { return "WEAK_POOL_ONLY"; }\n',
  );

  const result = await state.run(["connection pool"]);
  assert.match(result.stdout, /connectionPool/);
  assert.match(result.stdout, /COMPLETE_KEYWORD_EVIDENCE/);
  assert.doesNotMatch(result.stdout, /WEAK_POOL_ONLY/);
  assertKeywordResult(result);
});

test("cold keyword evidence can span adjacent source lines but not distant functions or different files", async (t) => {
  const state = await fixture(t);
  const adjacent = join(state.root, "adjacent.ts");
  await writeFile(
    adjacent,
    [
      "export function scheduleAttempt() {",
      "  const retryCount = 2;",
      "  const timeoutMillis = 400;",
      "  return retryCount + timeoutMillis;",
      "}",
      "",
    ].join("\n"),
  );
  await writeFile(
    join(state.root, "far.ts"),
    [
      'export function retryOnly() { return "DISTANT_RETRY_ONLY"; }',
      ...Array(40).fill(""),
      'export function timeoutOnly() { return "DISTANT_TIMEOUT_ONLY"; }',
      "",
    ].join("\n"),
  );
  await writeFile(
    join(state.root, "retry.ts"),
    'export const retryOnly = "CROSS_FILE_RETRY_ONLY";\n',
  );
  await writeFile(
    join(state.root, "timeout.ts"),
    'export const timeoutOnly = "CROSS_FILE_TIMEOUT_ONLY";\n',
  );
  await writeFile(
    join(state.root, "neighbors.ts"),
    [
      'export function retryOnly() { return "NEIGHBOR_RETRY_ONLY"; }',
      'export function timeoutOnly() { return "NEIGHBOR_TIMEOUT_ONLY"; }',
      "",
    ].join("\n"),
  );

  const result = await state.run(["retry timeout"]);
  assert.match(result.stdout, /adjacent\.ts/);
  assert.match(result.stdout, /retryCount = 2/);
  assert.match(result.stdout, /timeoutMillis = 400/);
  assert.doesNotMatch(result.stdout, /DISTANT_|CROSS_FILE_|NEIGHBOR_/);
  assertKeywordResult(result);

  await rm(adjacent);
  const unrelated = await state.run(["retry timeout"]);
  assert.equal(unrelated.stdout, incompleteEmpty);
  assert.match(unrelated.stderr, /semantic index is not ready/);
});

test("keyword windows do not join adjacent functions beyond the structural enrichment file budget", async (t) => {
  const state = await fixture(t);
  // More than the 25-code-file enrichment budget. Unenriched windows must not
  // claim that neighboring functions share an owner just because rg context
  // contains both keywords. Every individual matched source line has one term.
  for (let index = 0; index < 32; index++) {
    await writeFile(
      join(state.root, `boundary-${String(index).padStart(2, "0")}.ts`),
      [
        `export function retryOnly${index}() { return "RETRY_ONLY_${index}"; }`,
        `export function timeoutOnly${index}() { return "TIMEOUT_ONLY_${index}"; }`,
        "",
      ].join("\n"),
    );
  }
  const result = await state.run(["retry timeout"]);
  assert.equal(result.stdout, incompleteEmpty);
  assert.match(result.stderr, /semantic index is not ready/);
});

test("cold keywords respect the current directory, globs and ignored paths and reread added, modified and deleted files", async (t) => {
  const state = await fixture(t);
  const scope = join(state.root, "scope");
  await mkdir(scope);
  // rg only applies .gitignore by default inside a repository. This marker is
  // enough for discovery; no git add, index or HEAD exists in the fixture.
  await mkdir(join(scope, ".git"));
  await writeFile(join(scope, ".gitignore"), "ignored.ts\n");
  const current = join(scope, "current.ts");
  await writeFile(
    current,
    'export function connectionPool() { return "INITIAL_SOURCE"; }\n',
  );
  for (const [path, marker] of [
    [join(state.root, "outside.ts"), "OUTSIDE_CWD"],
    [join(scope, "ignored.ts"), "IGNORED_SOURCE"],
    [join(scope, "excluded.ts"), "EXCLUDED_SOURCE"],
    [join(scope, "notes.md"), "EXCLUDED_TYPE"],
  ]) {
    await writeFile(path, `connectionPool ${marker}\n`);
  }
  const args = ["connection pool", "-g", "!notes.md", "-g", "!excluded.ts"];
  const first = await state.run(args, scope);
  assert.match(first.stdout, /INITIAL_SOURCE/);
  assert.doesNotMatch(first.stdout, /OUTSIDE_CWD|IGNORED_SOURCE|EXCLUDED_/);
  assertKeywordResult(first);

  // An explicit positive rg glob intentionally overrides .gitignore. Keep
  // this separate from the default-ignore assertion; changing that expectation
  // is a fixture contract correction, not a product filtering change.
  const explicitlyIncluded = await state.run(
    ["connection pool", "-g", "*.ts", "-g", "!excluded.ts"],
    scope,
  );
  assert.match(explicitlyIncluded.stdout, /INITIAL_SOURCE/);
  assert.match(explicitlyIncluded.stdout, /IGNORED_SOURCE/);
  assert.doesNotMatch(explicitlyIncluded.stdout, /OUTSIDE_CWD|EXCLUDED_/);
  assertKeywordResult(explicitlyIncluded);

  // No Git metadata or git add is needed: searches observe the live tree.
  await writeFile(
    current,
    'export function connectionPool() { return "UPDATED_SOURCE"; }\n',
  );
  const added = join(scope, "added.ts");
  await writeFile(
    added,
    'export function connectionPool() { return "NEW_SOURCE"; }\n',
  );
  const updated = await state.run(args, scope);
  assert.match(updated.stdout, /UPDATED_SOURCE/);
  assert.match(updated.stdout, /NEW_SOURCE/);
  assert.doesNotMatch(
    updated.stdout,
    /INITIAL_SOURCE|IGNORED_SOURCE|EXCLUDED_/,
  );
  assertKeywordResult(updated);

  await rm(current);
  await rm(added);
  const deleted = await state.run(args, scope);
  assert.equal(deleted.stdout, incompleteEmpty);
  assert.match(deleted.stderr, /semantic index is not ready/);
});

test("cold keyword search finds later same-line owners without combining unrelated functions", async (t) => {
  const state = await fixture(t);
  const path = join(state.root, "same-line.ts");
  for (const prefix of ["", 'const café = "😀"; ']) {
    await writeFile(
      path,
      `${prefix}export function retryOnly() {} export function retryTimeout() { return "LATER_COMPLETE"; }\r\n`,
    );
    const found = await state.run(["retry timeout"]);
    assertKeywordResult(found);
    assert.match(found.stdout, /retryTimeout/);
    assert.match(found.stdout, /LATER_COMPLETE/);
    assert.doesNotMatch(found.stdout, /retryOnly|const café/);

    await writeFile(
      path,
      `${prefix}export function retryOnly() {} export function timeoutOnly() {}\r\n`,
    );
    const unrelated = await state.run(["retry timeout"]);
    assert.equal(unrelated.stdout, incompleteEmpty);
  }
  await writeFile(
    path,
    'export function retryTimeout() { return "FIRST_OWNER"; } export function timeoutRetry() { return "SECOND_OWNER"; }\n',
  );
  const distinct = await state.run(["retry timeout"]);
  assertKeywordResult(distinct);
  assert.equal(distinct.stdout.match(/matchedBy=keyword\b/g)?.length, 2);
  assert.equal(distinct.stdout.match(/FIRST_OWNER/g)?.length, 1);
  assert.equal(distinct.stdout.match(/SECOND_OWNER/g)?.length, 1);
});

test("keyword defaults do not broaden exact identifiers or managed fixed-string ripgrep", async (t) => {
  const state = await fixture(t);
  await writeFile(
    join(state.root, "exact.ts"),
    'export function connectionPool() { return "EXACT_DEFINITION"; }\n',
  );
  await writeFile(
    join(state.root, "extra.ts"),
    'export function connectionPoolExtra() { return "LONGER_IDENTIFIER"; }\n',
  );
  const exact = await state.run(["connectionPool"]);
  assert.match(exact.stdout, /EXACT_DEFINITION/);
  assert.doesNotMatch(exact.stdout, /LONGER_IDENTIFIER|matchedBy=keyword\b/);
  assert.equal(exact.stderr, "");

  const phrase = await state.run(["--rg", "-F", "connection pool"]);
  assert.equal(phrase.stdout, "No matches.\n");
  assert.equal(phrase.stderr, "");
  const substring = await state.run(["--rg", "-F", "connectionPool"]);
  assert.match(substring.stdout, /EXACT_DEFINITION/);
  assert.match(substring.stdout, /LONGER_IDENTIFIER/);
  assert.doesNotMatch(substring.stdout, /matchedBy=keyword\b/);
  assert.equal(substring.stderr, "");
});
