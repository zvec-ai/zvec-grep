import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import test from "node:test";
import { parseArgs } from "../../dist/cli/args.js";
import { normalizeCliQueries } from "../../dist/cli/query-input.js";

test("ordinary positional words describe one query, including former commands", () => {
  for (const words of [
    ["connection", "pool", "timeout"],
    ["query", "xxx"],
    ["search", "authorization"],
    ["index", "install"],
    ["为什么", "AuthService", "刷新失败"],
  ]) {
    const parsed = parseArgs(words);
    assert.equal(parsed.command, "query");
    assert.deepEqual(normalizeCliQueries(parsed.positionals), [
      words.join(" "),
    ]);
  }
});

test("quoted and unquoted text have the same default search intent", () => {
  assert.deepEqual(normalizeCliQueries(["connection pool timeout"]), [
    "connection pool timeout",
  ]);
  assert.deepEqual(normalizeCliQueries(["connection", "pool", "timeout"]), [
    "connection pool timeout",
  ]);
  // Literal quote characters passed through argv are search text, not hidden
  // routing operators. Shell quote syntax itself is no longer available.
  assert.deepEqual(normalizeCliQueries(['say "hello"']), ['say "hello"']);
});

test(
  "real shell single and double quotes produce identical argv",
  {
    skip: process.platform === "win32",
  },
  () => {
    const script =
      'printf "%s\\n" \'connection pool timeout\' "connection pool timeout"';
    const lines = execFileSync("/bin/sh", ["-c", script], { encoding: "utf8" })
      .trim()
      .split("\n");
    assert.deepEqual(lines, [
      "connection pool timeout",
      "connection pool timeout",
    ]);
  },
);

test("multiple query groups are explicit and empty queries are omitted", () => {
  assert.deepEqual(
    normalizeCliQueries(["connection", "pool"], ["socket timeout", " "]),
    ["connection pool", "socket timeout"],
  );
  assert.deepEqual(normalizeCliQueries([], ["first", "second"]), [
    "first",
    "second",
  ]);
  assert.deepEqual(normalizeCliQueries([" "]), []);
});
