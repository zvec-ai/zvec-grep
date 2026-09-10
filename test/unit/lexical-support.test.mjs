import assert from "node:assert/strict";
import test from "node:test";
import { lexicalSupport } from "../../dist/engine/pipeline/search/lexical-support.js";

function candidate(id, text = "", path = "src/other.ts", name = "other") {
  return {
    id,
    file: { relativePath: path },
    entity: {
      id,
      content: { kind: "text", text },
      metadata: { kind: "code", symbolName: name, scope: null },
    },
    fragments: [],
  };
}

test("lexical support recognizes paths, camel-case, acronym boundaries and plural code words", () => {
  const relevant = candidate(
    "relevant",
    "maxInputTokens HTTPRequests",
    "src/input-budget.ts",
    "indexChunkOptions",
  );
  const unrelated = candidate(
    "unrelated",
    "return response",
    "src/response.ts",
    "sendResponse",
  );
  for (const query of [
    "input token budget",
    "HTTP requests",
    "chunk options",
  ]) {
    const support = lexicalSupport([relevant, unrelated], query);
    assert.ok(support.get("relevant") > support.get("unrelated"), query);
    assert.ok([...support.values()].every((value) => value >= 0 && value <= 1));
  }
  assert.ok(
    lexicalSupport(
      [candidate("retry", "policies", "retry.ts", "retryRequests")],
      "retry policy",
    ).get("retry") > 0,
  );
  assert.ok(
    lexicalSupport(
      [candidate("suggest", "", "command.go", "findSuggestions")],
      "suggest command",
    ).get("suggest") > 0,
  );
});

test("support neither rewards repeated terms nor combines independent source windows", () => {
  const split = candidate("split");
  split.fragments = [
    { id: "a", content: { kind: "text", text: "network" } },
    { id: "b", content: { kind: "text", text: "backoff" } },
  ];
  const together = candidate("together", "network backoff");
  const support = lexicalSupport([split, together], "network backoff");
  assert.equal(support.get("split"), 0.25);
  assert.equal(support.get("together"), 0.5);
  split.fragments.push(split.fragments[0]);
  together.entity.content.text = "network backoff ".repeat(100);
  assert.deepEqual(
    lexicalSupport([split, together], "network backoff"),
    support,
  );
});

test("rare support counts more than terms present in every candidate", () => {
  const candidates = Array.from({ length: 12 }, (_, i) =>
    candidate(String(i), "common"),
  );
  candidates[0].entity.metadata.symbolName = "rare";
  candidates[1].entity.metadata.symbolName = "common";
  const support = lexicalSupport(candidates, "rare common");
  assert.ok(support.get("0") > support.get("1"));
});

test("ranking hints are bounded and do not mutate candidates or invent Unicode prefix matches", () => {
  const unicode = candidate("unicode", "网络重试");
  unicode.entity.metadata = {
    kind: "markdown",
    heading: "网络重试",
    scope: "指南",
  };
  const image = candidate("image");
  image.entity.content = {
    kind: "image",
    format: "png",
    data: new Uint8Array(),
  };
  const inputs = [unicode, image];
  const before = structuredClone(inputs);
  assert.equal(lexicalSupport(inputs, "网络重试").get("unicode"), 1);
  assert.equal(lexicalSupport(inputs, "网络").get("unicode"), 0);
  assert.equal(lexicalSupport(inputs, "网络重试").get("image"), 0);
  assert.deepEqual(inputs, before);
  for (const query of [
    "",
    "the and how",
    "a".repeat(8193),
    Array.from({ length: 65 }, (_, i) => `term${i}`).join(" "),
  ])
    assert.equal(lexicalSupport(inputs, query).size, 0);
  assert.equal(lexicalSupport([], "query").size, 0);
  const huge = candidate("huge", " ".repeat(32768) + "sentinel");
  assert.equal(lexicalSupport([huge], "sentinel").get("huge"), 0);
});
