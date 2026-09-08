import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import {
  addLiveKeywordMatches,
  keywordQuery,
  keywordSourceWindow,
  keywordWindowScore,
} from "../../dist/cli/live-keywords.js";
import {
  contextWarningLines,
  formatCliContextResult,
  printCliContextResult,
} from "../../dist/cli/format/context.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

test("keyword fallback preserves exact-query grammar and bounds ambiguous input", () => {
  for (const query of [
    "connectionPool",
    "src/auth.ts",
    "cache",
    "如何取消队列任务",
    "where is the",
    "x y",
    "a".repeat(1025),
    Array.from({ length: 13 }, (_, i) => `term${i}`).join(" "),
  ]) {
    assert.equal(keywordQuery(query), undefined, query);
  }
  assert.deepEqual(
    keywordQuery("Where does the connection pool live?").groups,
    [["connection"], ["pool"], ["live"]],
  );
  assert.deepEqual(keywordQuery("retry retry timeout").groups, [
    ["retry"],
    ["timeout"],
  ]);
});

test("keyword scoring matches code subwords but not arbitrary substrings", () => {
  const query = keywordQuery("input token budget");
  assert.equal(
    keywordWindowScore(query, "const maxInputTokens = model.inputBudget;"),
    1,
  );
  assert.equal(keywordWindowScore(query, "const inputBudget = limit;"), 2 / 3);
  assert.equal(keywordWindowScore(query, "token token token token"), 0);
  assert.equal(
    keywordWindowScore(keywordQuery("cache entry"), "cachet entryway"),
    0,
  );
  assert.equal(
    keywordWindowScore(keywordQuery("HTTP response"), "httpResponse"),
    1,
  );
  assert.equal(
    keywordWindowScore(query, "input tokens budget" + "x".repeat(16384)),
    0,
  );
});

test("keyword discovery and scoring agree on simple plural normalization", () => {
  for (const [text, source] of [
    ["policies rules", "policyRule"],
    ["policy rule", "policies rules"],
    ["input tokens", "maxInputToken"],
  ]) {
    const query = keywordQuery(text);
    assert.equal(keywordWindowScore(query, source), 1);
    assert.ok(
      query.patterns.every((pattern) => new RegExp(pattern, "i").test(source)),
    );
  }
});

test("one identifier split into subwords cannot satisfy a mixed-language question", () => {
  for (const identifier of [
    "Model2Vec",
    "model2_vec",
    "max_input_tokens",
    "$Model2Vec",
  ]) {
    for (const query of [
      `${identifier} 如何取消队列任务`,
      `${identifier}如何取消队列任务`,
    ]) {
      assert.equal(
        keywordWindowScore(keywordQuery(query), `class ${identifier} {}`),
        0,
      );
    }
  }
  assert.equal(keywordWindowScore(keywordQuery("café pool"), "caféPool"), 1);
});

test("keyword scoring clips context at the actual enclosing function", () => {
  const item = {
    range: {
      kind: "text",
      startLine: 1,
      endLine: 4,
      startOffset: 0,
      endOffset: 1,
    },
    content: "function retryOnly() {}\n\nfunction timeoutOnly() {}\n",
    excerptRange: { kind: "text", startLine: 1, endLine: 1 },
    container: { range: { kind: "text", startLine: 1, endLine: 1 } },
  };
  const clipped = keywordSourceWindow(item);
  assert.equal(
    keywordWindowScore(keywordQuery("retry timeout"), item.content),
    1,
  );
  assert.equal(
    keywordWindowScore(keywordQuery("retry timeout"), clipped.content),
    0,
  );
  assert.equal(clipped.range.startLine, 1);
  assert.equal(clipped.range.endLine, 1);
  assert.equal(item.range.endLine, 4);
  assert.equal(
    keywordSourceWindow({ ...item, container: undefined }).content,
    "function retryOnly() {}",
  );
});

test("keyword anchors deduplicate returned source windows without replacing literal evidence or different lines", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-keyword-dedup-");
  const path = join(root, "notes.txt");
  const content = "permission validation permission validation";
  await writeFile(path, `${content}\n${content}\n`);
  const literalItem = {
    kind: "lexical_match",
    rank: 1,
    status: "fresh",
    matchedBy: "lexical",
    file: { absolutePath: path, relativePath: "notes.txt" },
    // A literal match covers only part of the line; content is the returned
    // complete source line. Keyword anchors have other match columns.
    range: {
      kind: "text",
      startLine: 1,
      endLine: 1,
      startOffset: 11,
      endOffset: 21,
    },
    content,
  };
  const result = await addLiveKeywordMatches(
    {
      query: "permission validation",
      root,
      source: "rg",
      coverage: "ranked_sample",
      items: [literalItem],
      diagnostics: {},
    },
    [{ options: {}, displayPath: () => "notes.txt" }],
    () => true,
    10,
  );
  assert.equal(result.items.length, 2);
  assert.deepEqual(result.items[0], literalItem);
  assert.equal(result.items[1].matchedBy, "keyword");
  assert.equal(result.items[1].range.startLine, 2);
  assert.equal(result.items[1].range.endLine, 2);
  assert.equal(result.items[1].content, content);
});

test("keyword source and incomplete coverage remain visible to humans and non-TTY agents", () => {
  const result = {
    query: "connection pool",
    root: "/repo",
    source: "rg",
    coverage: "ranked_sample",
    items: [
      {
        kind: "lexical_match",
        rank: 1,
        status: "fresh",
        matchedBy: "keyword",
        file: { absolutePath: "/repo/pool.ts", relativePath: "pool.ts" },
        range: {
          kind: "text",
          startLine: 2,
          endLine: 2,
          startOffset: 0,
          endOffset: 60,
        },
        content:
          "export function connectionPool() { return reuseConnections(); }",
      },
    ],
    diagnostics: {
      keywords: {
        terms: ["connection", "pool"],
        candidates: 1,
        truncated: true,
      },
    },
  };
  const plain = formatCliContextResult(result, { color: "never" });
  assert.match(plain, /matchedBy=keyword/);
  assert.match(plain, /2:.*reuseConnections/);
  const lines = [];
  const originalLog = console.log;
  try {
    console.log = (...parts) => lines.push(parts.join(" "));
    printCliContextResult(result, { human: true, color: "never" });
  } finally {
    console.log = originalLog;
  }
  assert.match(lines.join("\n"), /matchedBy=keyword/);
  assert.match(lines.join("\n"), /reuseConnections/);
  assert.deepEqual(contextWarningLines(result), [
    "warning: keyword search reached a scan or resource limit; text coverage is incomplete",
  ]);
});
