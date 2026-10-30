import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";
import {
  parseVisibleResponse,
  scoreResponse,
  validateSearchRoute,
  VisibleFormatError,
} from "../engines/zg/parse.mjs";
import { scoreFileRetrieval } from "../metrics/files.mjs";

const sha = (s) => createHash("sha256").update(s).digest("hex");
const response = (body, extra = {}) => ({
  content: [{ type: "text", text: `freshness: fresh\n${body}` }],
  ...extra,
});
const anchor = (text = "def wanted():", start = 20) => ({
  start_line: start,
  end_line: start + text.split("\n").length - 1,
  text,
  sha256: sha(text),
});
function target(id = "wanted", options = {}) {
  return {
    id,
    path: "pkg/main.py",
    kind: "symbol",
    symbol: "wanted",
    source_sha256: "a".repeat(64),
    anchors: [anchor()],
    role: "accepted",
    relevance_reason: "Defines the queried mechanism",
    ...options,
  };
}
function gold(
  targets = [target()],
  ndcg = {
    enabled: false,
    reason: "Partial positive entry labels",
    groups: [],
  },
) {
  return {
    schema_version: 1,
    gold_version: "sweqa20-entry-v1",
    task_id: "example:1",
    question_sha256: "b".repeat(64),
    repository: "example/repo",
    repository_commit: "c".repeat(40),
    status: "reviewed",
    targets,
    ndcg,
    review: { proposer: "a", reviewer: "b" },
  };
}
const item = (
  rank,
  body = "source:\n20\tdef wanted():",
  location = "pkg/main.py:20-40",
) => `#${rank} matchedBy=fts+vector ${location}\n${body}`;
const filler = (rank) =>
  item(rank, "source:\n2\tdef other():", "pkg/main.py:2-5");
const atRank = (rank) =>
  response(
    [
      ...Array.from({ length: rank - 1 }, (_, i) => filler(i + 1)),
      item(rank),
    ].join("\n\n"),
  );

const routeItems = (routes) =>
  parseVisibleResponse(
    response(
      routes
        .map((route, index) =>
          item(index + 1).replace("matchedBy=fts+vector", `matchedBy=${route}`),
        )
        .join("\n\n"),
    ),
  ).items;

const compactResponse = (body, freshness = "fresh") => ({
  content: [{ type: "text", text: `freshness: ${freshness}\n${body}` }],
});

test("compact MCP output preserves ranks and visible source without inventing route evidence", () => {
  const reply = compactResponse(
    [
      "#1 pkg/main.py:20-40",
      "symbol: [function] wanted scope: Source",
      "  20: def wanted():",
      "  21:     return True",
      "",
      "#2 notes/design.md:file",
      "heading: Design",
      "heading_level: 1",
      "scope: Search / Design",
      "  # Design",
      "  ",
      "  Retain local evidence.",
      "",
    ].join("\n"),
  );
  const parsed = parseVisibleResponse(reply);
  assert.equal(parsed.freshness, "fresh");
  assert.deepEqual(
    parsed.items.map((item) => item.rank),
    [1, 2],
  );
  assert.deepEqual(
    parsed.items.map((item) => item.matched_by),
    [null, null],
  );
  assert.deepEqual(parsed.items[0].source_lines, [
    { line: 20, text: "def wanted():" },
    { line: 21, text: "    return True" },
  ]);
  assert.deepEqual(parsed.items[1].unnumbered_source, [
    "# Design",
    "",
    "Retain local evidence.",
  ]);
  assert.deepEqual(parsed.items[0].outline, []);
  for (const mode of ["hybrid", "fts", "vector"])
    assert.doesNotThrow(() => validateSearchRoute(parsed.items, mode));
  const score = scoreResponse(reply, gold());
  assert.equal(score.status, "scored");
  assert.equal(
    scoreFileRetrieval(score.items, [{ path: "pkg/main.py" }]).hit_at_1,
    1,
  );
});

test("compact empty results and global CLI diagnostics remain explicit public evidence", () => {
  for (const label of ["No matches.", "No searchable files."])
    for (const diagnostics of [
      "",
      "status: possibly_stale\n",
      "status: possibly_stale\nresults: served_from_current_index\nbackground_refresh: running (2/3)\n",
    ]) {
      const parsed = parseVisibleResponse(
        compactResponse(
          `${label}\n${diagnostics}`,
          diagnostics ? "possibly_stale" : "fresh",
        ),
      );
      assert.deepEqual(parsed.items, []);
      assert.equal(parsed.freshness, diagnostics ? "possibly_stale" : "fresh");
      assert.equal(
        parsed.empty_reason,
        label === "No matches." ? "no_matches" : "no_searchable_files",
      );
    }
  const parsed = parseVisibleResponse(
    compactResponse(
      "#1 pkg/main.py:20-40\nstatus: possibly_stale\n  20: def wanted():\n  …\nstatus: possibly_stale\nresults: served_from_current_index\nbackground_refresh: idle\n",
      "possibly_stale",
    ),
  );
  assert.equal(parsed.freshness, "possibly_stale");
  assert.equal(parsed.items[0].metadata[0], "status: possibly_stale");
  assert.deepEqual(parsed.items[0].source_lines, [
    { line: 20, text: "def wanted():" },
  ]);
});

test("compact output keeps the required freshness header and accepts leading global diagnostics", () => {
  for (const body of [
    "No matches.\n",
    "#1 pkg/main.py:20-40\n  20: def wanted():\n",
  ]) {
    assert.throws(
      () => parseVisibleResponse({ content: [{ type: "text", text: body }] }),
      /Missing public MCP freshness header/,
    );
    const parsed = parseVisibleResponse(
      compactResponse(
        `status: possibly_stale\nresults: served_from_current_index\nbackground_refresh: running (2/3)\n\n${body}`,
        "possibly_stale",
      ),
    );
    assert.equal(parsed.freshness, "possibly_stale");
    assert.equal(parsed.items.length, body.startsWith("#1") ? 1 : 0);
  }
});

test("compact framing rejects malformed ranks, source coordinates and diagnostics", () => {
  for (const body of [
    "",
    "#2 pkg/main.py:20-40\n  20: def wanted():\n",
    "#1 ../main.py:20-40\n  20: def wanted():\n",
    "#1 pkg/main.py:20-40\n  19: def wanted():\n",
    "#1 pkg/main.py:20-40\n  20: def wanted():\n  20: duplicate\n",
    "#1 pkg/main.py:20-40\nunknown renderer line\n",
    "No matches.\nstatus: possibly_stale\nresults: served_from_current_index\n",
    "No matches.\nbackground_refresh: running\n",
    "status: possibly_stale\nresults: served_from_current_index\n\nNo matches.\n",
  ])
    assert.throws(
      () => parseVisibleResponse(compactResponse(body)),
      VisibleFormatError,
    );
});

for (const [mode, routes] of [
  ["hybrid", ["fts", "vector", "fts+vector"]],
  ["fts", ["fts", "fts"]],
  ["vector", ["vector", "vector"]],
]) {
  test(`${mode} accepts its native indexed routes without changing public items`, () => {
    const items = routeItems(routes);
    const snapshot = structuredClone(items);
    assert.doesNotThrow(() => validateSearchRoute(items, mode));
    assert.deepEqual(items, snapshot);
  });
}

test("single-route arms reject results credited to another route", () => {
  for (const [mode, unexpected] of [
    ["fts", "vector"],
    ["fts", "fts+vector"],
    ["vector", "fts"],
    ["vector", "fts+vector"],
  ])
    assert.throws(
      () => validateSearchRoute(routeItems([mode, unexpected]), mode),
      (error) =>
        error instanceof assert.AssertionError &&
        error.message.includes(`${mode}: unexpected matchedBy=${unexpected}`) &&
        error.message.includes("at rank 2"),
    );
});

test("lexical remains a valid public format but is excluded from all indexed benchmark arms", () => {
  const items = routeItems(["lexical"]);
  assert.equal(items[0].matched_by, "lexical");
  for (const mode of ["hybrid", "fts", "vector"])
    assert.throws(
      () => validateSearchRoute(items, mode),
      /unexpected matchedBy=lexical/,
    );
});

test("explicit empty results are valid for every planned search route", () => {
  for (const label of ["No matches.", "No searchable files."])
    for (const mode of ["hybrid", "fts", "vector"])
      assert.doesNotThrow(() =>
        validateSearchRoute(parseVisibleResponse(response(label)).items, mode),
      );
});

test("route validation rejects unknown modes and missing route evidence", () => {
  for (const mode of [undefined, "lexical", "__proto__", ""])
    assert.throws(() => validateSearchRoute([], mode), /unknown search mode/);
  for (const mode of ["hybrid", "fts", "vector"]) {
    assert.throws(
      () => validateSearchRoute(null, mode),
      /missing public parsed items/,
    );
    assert.throws(
      () => validateSearchRoute([{ rank: 1 }], mode),
      /unexpected matchedBy=undefined at rank 1/,
    );
  }
});

const removedFields = [
  "first_hit_rank",
  "hit_at_1",
  "hit_at_5",
  "hit_at_10",
  "rr_at_10",
  "ndcg_at_5",
  "ndcg_at_10",
  "target_matches",
];
const assertNoLegacyScores = (score) => {
  for (const key of removedFields)
    assert.equal(Object.hasOwn(score, key), false, key);
};

for (const rank of [1, 5, 10]) {
  test(`public parser preserves all ${rank} native ranks without file compaction`, () => {
    const parsed = scoreResponse(atRank(rank), gold());
    assert.equal(parsed.status, "scored");
    assert.equal(parsed.execution_status, "success");
    assert.equal(parsed.items.length, rank);
    assert.deepEqual(
      parsed.items.map((item) => item.rank),
      Array.from({ length: rank }, (_, i) => i + 1),
    );
    assertNoLegacyScores(parsed);
  });
}

test("file relevance is independent of source text, outlines and declaration location", () => {
  const files = [{ path: "pkg/main.py" }];
  for (const body of [
    item(1, "source:\n20\tdef other():"),
    item(1, "symbol: function wanted"),
    item(
      1,
      "outline:\nclass Parent:\n\nmembers:\n- function wanted",
      "pkg/main.py:1-400",
    ),
    item(1, "outline:\ndef wanted():", "pkg/main.py:19-40"),
    item(1, "source:\n24\t    state = loo...\n..."),
  ]) {
    const parsed = scoreResponse(response(body), gold());
    assert.equal(parsed.status, "scored");
    assert.equal(scoreFileRetrieval(parsed.items, files).hit_at_1, 1);
    assertNoLegacyScores(parsed);
  }
});

test("lexical line markers, whitespace and matched source ranges remain exact public evidence", () => {
  const parsed = scoreResponse(
    response(
      item(
        1,
        "matched: 28\nsource:\n27-\t  before\n28:\treturn result\n29-\t\tafter",
      ),
    ),
    gold(),
  );
  assert.equal(parsed.status, "scored");
  assert.deepEqual(parsed.items[0].matched_range, {
    kind: "text",
    start_line: 28,
    end_line: 28,
  });
  assert.deepEqual(parsed.items[0].source_lines, [
    { line: 27, text: "  before" },
    { line: 28, text: "return result" },
    { line: 29, text: "\tafter" },
  ]);
  assertNoLegacyScores(parsed);
});

test("Rust MCP default outline and unnumbered source remain public evidence", () => {
  const parsed = parseVisibleResponse(
    response(
      [
        "#1 matchedBy=vector score=0.75 pkg/main.py:20-40",
        "outline: function wanted",
        "source:",
        "  def wanted():",
        "    return result",
      ].join("\n"),
    ),
  );
  assert.deepEqual(parsed.items[0].outline, ["function wanted"]);
  assert.deepEqual(parsed.items[0].unnumbered_source, [
    "def wanted():",
    "  return result",
  ]);
});

test("Rust MCP served-from-current-index framing is parsed without changing ranks", () => {
  const parsed = parseVisibleResponse({
    content: [
      {
        type: "text",
        text: [
          "freshness: served_from_current_index",
          "background_refresh: scheduled",
          "#1 matchedBy=fts pkg/main.py:20-40",
          "source:",
          "  def wanted():",
        ].join("\n"),
      },
    ],
  });
  assert.equal(parsed.freshness, "served_from_current_index");
  assert.equal(parsed.items[0].rank, 1);
});

test("all suites accept at most one empty source line after the public range", () => {
  const body = (last) =>
    response(
      item(
        1,
        `source:\n20\tdef wanted():\n21\t    return True\n22\t${last}`,
        "pkg/main.py:20-21",
      ),
    );
  const accepted = scoreResponse(body(""), gold());
  assert.equal(accepted.status, "scored");
  assert.equal(accepted.items[0].path, "pkg/main.py");
  for (const bad of [
    body("unexpected"),
    response(
      item(
        1,
        "source:\n20\tdef wanted():\n21\t    return True\n22\t\n23\t",
        "pkg/main.py:20-21",
      ),
    ),
  ]) {
    assert.throws(() => parseVisibleResponse(bad), VisibleFormatError);
    assert.equal(scoreResponse(bad, gold()).status, "harness_invalid");
  }
});

test("explicit empty is valid; missing, unrelated and malformed formats invalidate the harness", () => {
  for (const label of ["No matches.", "No searchable files."]) {
    const parsed = scoreResponse(response(label), gold());
    assert.equal(parsed.status, "scored");
    assert.deepEqual(parsed.items, []);
    assertNoLegacyScores(parsed);
  }
  for (const bad of [
    response(""),
    response("Nothing found"),
    response('{"items": []}'),
    response(item(2)),
    response(`${item(1)}\nunknown renderer line`),
    { content: [] },
    { content: [{ type: "text", text: item(1) }] },
    response(item(1, "source:\n20\tdef wanted():\n20\tdef wanted():")),
    response(item(1, "source:\n100\tdef wanted():")),
    response(item(1, "", "../pkg/main.py:20-40")),
    response(item(1) + "\r"),
    response(item(1) + "\x1b[0m"),
  ]) {
    assert.throws(() => parseVisibleResponse(bad), VisibleFormatError);
    const parsed = scoreResponse(bad, gold());
    assert.equal(parsed.status, "harness_invalid");
    assertNoLegacyScores(parsed);
  }
});

test("product failures and unreviewed Gold retain eligibility but never emit legacy metric fields", () => {
  const error = {
    isError: true,
    content: [{ type: "text", text: "Index unavailable" }],
  };
  const failed = scoreResponse(error, gold());
  assert.equal(failed.status, "product_error");
  assert.equal(failed.execution_status, "product_error");
  assert.deepEqual(failed.items, []);
  assertNoLegacyScores(failed);
  for (const status of ["unknown", "disputed"]) {
    const parsed = scoreResponse(error, { ...gold(), status });
    assert.equal(parsed.status, `gold_${status}`);
    assertNoLegacyScores(parsed);
  }
});

test("frozen Gold integrity is still validated even though anchors and groups are no longer scored", () => {
  const reviewed = gold([target()], {
    enabled: true,
    groups: [{ id: "g1", target_ids: ["wanted"] }],
  });
  const parsed = scoreResponse(
    response(item(1, "source:\n20\tdef unrelated():")),
    reviewed,
  );
  assert.equal(parsed.status, "scored");
  assertNoLegacyScores(parsed);
  for (const mutate of [
    (g) => {
      g.targets[0].anchors[0].sha256 = "0".repeat(64);
    },
    (g) => {
      g.targets[0].source_sha256 = "bad";
    },
    (g) => {
      g.targets[0].path = "../main.py";
    },
    (g) => {
      g.ndcg.groups[0].target_ids = ["missing"];
    },
  ]) {
    const corrupt = structuredClone(reviewed);
    mutate(corrupt);
    const invalid = scoreResponse(atRank(1), corrupt);
    assert.equal(invalid.status, "harness_invalid");
    assert.match(invalid.invalid_reason, /^gold_invalid:/);
    assertNoLegacyScores(invalid);
  }
});

test("visible and structured hashes remain separate; hidden source never augments public items", () => {
  const a = scoreResponse(
    response(item(1), { structuredContent: { a: 1, b: 2 } }),
    gold(),
  );
  const b = scoreResponse(
    response(item(1), { structuredContent: { b: 2, a: 1 } }),
    gold(),
  );
  assert.equal(a.visible_output_sha256, b.visible_output_sha256);
  assert.equal(a.structured_output_sha256, b.structured_output_sha256);
  assert.equal(a.ranking_sha256, b.ranking_sha256);
  const hidden = scoreResponse(
    response(item(1, "symbol: function wanted"), {
      structuredContent: { content: "def wanted():" },
    }),
    gold(),
  );
  assert.deepEqual(hidden.items[0].source_lines, []);
  assert.deepEqual(hidden.items[0].outline, []);
  assertNoLegacyScores(hidden);
});

test("native non-code range remains a file result without invented source positions", () => {
  const parsed = parseVisibleResponse(
    response(item(1, "heading: Notes", "docs/notes.pdf:page:2")),
  );
  assert.equal(parsed.items[0].path, "docs/notes.pdf");
  assert.deepEqual(parsed.items[0].range, { kind: "other", label: "page:2" });
  assert.equal(
    scoreFileRetrieval(parsed.items, [{ path: "docs/notes.pdf" }]).hit_at_1,
    1,
  );
});
