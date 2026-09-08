import assert from "node:assert/strict";
import test from "node:test";
import { locations } from "../evaluation/output-locations.mjs";

test("evaluation counts rg matches and symbol blocks, not context preview lines", () => {
  assert.deepEqual(
    locations(
      [
        "src/auth.ts",
        "  5-\t// context",
        "  6:\tfunction auth() {}",
        "  7-\t// context",
        "  20-30 [function caller]",
        "    22:\tauth();",
        "src/other.ts",
        "  10:\tauth();",
      ].join("\n"),
    ),
    [
      { rank: 1, file: "src/auth.ts", start: 6, end: 6 },
      { rank: 2, file: "src/auth.ts", start: 20, end: 30 },
      { rank: 3, file: "src/other.ts", start: 10, end: 10 },
    ],
  );
});

test("evaluation recognizes indexed ranges, files, and real no-match output", () => {
  assert.deepEqual(
    locations("#1 matchedBy=vector src/auth.ts:7-14\n7\tfunction auth()"),
    [{ rank: 1, file: "src/auth.ts", start: 7, end: 14 }],
  );
  assert.deepEqual(locations("#1 matchedBy=path src/auth.ts\n"), [
    { rank: 1, file: "src/auth.ts" },
  ]);
  assert.deepEqual(locations("No matches.\n"), []);
});

test("evaluation recognizes known root and lib paths without treating source context as a filename", () => {
  const files = new Set(["lib/response.js", "args.go", "src/click/types.py"]);
  assert.deepEqual(
    locations(
      [
        "lib/response.js",
        "  352-\t// context",
        "  373:\tres.sendFile = function sendFile() {}",
        "args.go",
        "  107-114 [function ExactArgs]",
        "    110:\treturn nil",
        "#3 matchedBy=vector src/click/types.py:395-413",
        "#4 matchedBy=path args.go",
      ].join("\n"),
      files,
    ),
    [
      { rank: 1, file: "lib/response.js", start: 373, end: 373 },
      { rank: 2, file: "args.go", start: 107, end: 114 },
      { rank: 3, file: "src/click/types.py", start: 395, end: 413 },
      { rank: 4, file: "args.go" },
    ],
  );
});
