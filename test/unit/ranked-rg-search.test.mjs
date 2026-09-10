import assert from "node:assert/strict";
import { mkdir, utimes, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { performance } from "node:perf_hooks";
import test from "node:test";
import { runRgSearch } from "../../dist/engine/service/lexical.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

async function fixture(t, contents) {
  const root = await createTemporaryDirectory(t, "zvec-ranked-rg-");
  await writeFile(join(root, "matches.txt"), contents);
  return root;
}

test("ranked rg scores expanded context and rejects non-positive or non-finite scores", async (t) => {
  const scores = new Map([
    ["zero", 0],
    ["negative", -1],
    ["nan", NaN],
    ["infinity", Infinity],
    ["two", 2],
    ["five", 5],
  ]);
  const root = await fixture(
    t,
    [...scores.keys()].map((label) => `${label}\nneedle\n`).join(""),
  );
  const seen = [];
  const result = await runRgSearch({
    root,
    patterns: ["needle"],
    limit: 10,
    rgOptions: { beforeContext: 1 },
    rankItem(item) {
      seen.push(item);
      assert.equal(item.excerptRange.startLine, item.range.startLine + 1);
      return scores.get(item.content.split("\n")[0]);
    },
  });
  assert.equal(seen.length, 6);
  assert.deepEqual(
    result.items.map((item) => item.content),
    ["five\nneedle", "two\nneedle"],
  );
  assert.deepEqual(
    result.items.map((item) => item.rank),
    [1, 2],
  );
  assert.equal(result.diagnostics.truncated, false);
});

test("ranked rg keeps late strong matches after more than limit weak matches", async (t) => {
  const root = await fixture(
    t,
    [
      ...Array.from({ length: 100 }, (_, index) => `needle weak ${index}`),
      "needle strong",
      "needle strongest",
      "",
    ].join("\n"),
  );
  let seen = 0;
  const result = await runRgSearch({
    root,
    patterns: ["needle"],
    limit: 2,
    rankItem(item) {
      seen++;
      return item.content.endsWith("strongest")
        ? 100
        : item.content.endsWith("strong")
          ? 10
          : 1;
    },
  });
  assert.equal(seen, 102);
  assert.deepEqual(
    result.items.map((item) => item.content),
    ["needle strongest", "needle strong"],
  );
  assert.deepEqual(
    result.items.map((item) => item.rank),
    [1, 2],
  );
  assert.equal(result.diagnostics.truncated, true);
});

test("ranked rg preserves first appearance when scores tie", async (t) => {
  const root = await fixture(t, "needle first\nneedle second\nneedle third\n");
  const result = await runRgSearch({
    root,
    patterns: ["needle"],
    limit: 2,
    rankItem: () => 7,
  });
  assert.deepEqual(
    result.items.map((item) => item.content),
    ["needle first", "needle second"],
  );
  assert.equal(result.diagnostics.truncated, true);
});

test("all-occurrence anchors are opt-in and each JSON match event is parsed once", async (t) => {
  const root = await fixture(t, "needle first needle\nneedle second needle\n");
  const options = { root, patterns: ["needle"] };
  for (const extra of [
    {},
    { rankItem: () => 1 },
    { matchAllOccurrences: false },
  ]) {
    const legacy = await runRgSearch({ ...options, ...extra });
    assert.equal(legacy.items.length, 2);
    assert.ok(legacy.items.every((item) => item.range.startOffset === 0));
    assert.equal(legacy.diagnostics.truncated, false);
  }
  let parsedMatches = 0;
  const originalParse = JSON.parse;
  const parser = t.mock.method(JSON, "parse", (...args) => {
    const value = originalParse(...args);
    if (value?.type === "match") parsedMatches++;
    return value;
  });
  try {
    const all = await runRgSearch({ ...options, matchAllOccurrences: true });
    assert.equal(parsedMatches, 2);
    assert.deepEqual(
      all.items.map((item) => [item.range.startLine, item.range.startOffset]),
      [
        [1, 0],
        [1, 13],
        [2, 0],
        [2, 14],
      ],
    );
    assert.deepEqual(
      all.items.map((item) => item.rank),
      [1, 2, 3, 4],
    );
    assert.equal(all.diagnostics.truncated, false);
  } finally {
    parser.mock.restore();
  }
});

test("scanLimit counts match events, allowing both first-line anchors before truncation", async (t) => {
  const root = await fixture(t, "needle first needle\nneedle second needle\n");
  const options = { root, patterns: ["needle"], matchAllOccurrences: true };
  const oneLine = await runRgSearch({ ...options, scanLimit: 1 });
  assert.deepEqual(
    oneLine.items.map((item) => [item.range.startLine, item.range.startOffset]),
    [
      [1, 0],
      [1, 13],
    ],
  );
  assert.equal(oneLine.diagnostics.truncated, true);
  const twoLines = await runRgSearch({ ...options, scanLimit: 2 });
  assert.equal(twoLines.items.length, 4);
  assert.equal(twoLines.diagnostics.truncated, false);
});

test("the 64-submatch cap is per event and also applies without a ranker", async (t) => {
  const root = await fixture(
    t,
    `${"needle ".repeat(67)}\nneedle later needle\n`,
  );
  const result = await runRgSearch({
    root,
    patterns: ["needle"],
    matchAllOccurrences: true,
  });
  assert.equal(result.items.length, 66);
  assert.equal(
    result.items.filter((item) => item.range.startLine === 1).length,
    64,
  );
  assert.equal(
    result.items.filter((item) => item.range.startLine === 2).length,
    2,
  );
  assert.equal(result.items[63].range.startOffset, 63 * 7);
  assert.equal(result.diagnostics.truncated, true);
});

test("ranked top-200 counts anchors without deduplicating identical line content", async (t) => {
  const root = await fixture(t, `${"needle ".repeat(64)}\n`.repeat(4));
  let seen = 0;
  const result = await runRgSearch({
    root,
    patterns: ["needle"],
    matchAllOccurrences: true,
    limit: 200,
    rankItem() {
      seen++;
      return 1;
    },
  });
  assert.equal(seen, 256);
  assert.equal(result.items.length, 200);
  assert.equal(new Set(result.items.map((item) => item.content)).size, 1);
  assert.equal(result.items.at(-1).range.startLine, 4);
  assert.equal(result.items.at(-1).range.startOffset, 7 * 7);
  assert.equal(result.items.at(-1).rank, 200);
  assert.equal(result.diagnostics.truncated, true);
});

test("all anchors retain UTF-16 columns and CRLF lines for UTF-8 and UTF-16 files", async (t) => {
  const lines = ["🚀 café needle + needle", "第二 needle / needle"];
  const text = `${lines.join("\r\n")}\r\n`;
  const expected = lines.flatMap((line, index) => [
    [index + 1, line.indexOf("needle"), line.indexOf("needle") + 6],
    [index + 1, line.lastIndexOf("needle"), line.lastIndexOf("needle") + 6],
  ]);
  for (const contents of [
    text,
    Buffer.concat([Buffer.from([0xff, 0xfe]), Buffer.from(text, "utf16le")]),
  ]) {
    const root = await fixture(t, contents);
    const result = await runRgSearch({
      root,
      patterns: ["needle"],
      matchAllOccurrences: true,
    });
    assert.deepEqual(
      result.items.map((item) => [
        item.range.startLine,
        item.range.startOffset,
        item.range.endOffset,
      ]),
      expected,
    );
    assert.ok(result.items.every((item) => !item.content.includes("\r")));
    assert.equal(result.diagnostics.truncated, false);
  }
});

test("the deadline is rechecked after a rejected anchor before advancing within its line", async (t) => {
  const root = await fixture(t, "needle first needle later\n");
  let time = 0;
  let seen = 0;
  const clock = t.mock.method(performance, "now", () => time);
  try {
    const result = await runRgSearch({
      root,
      patterns: ["needle"],
      matchAllOccurrences: true,
      timeoutMs: 1_000,
      rankItem() {
        seen++;
        time = 1_001;
        return 0;
      },
    });
    assert.equal(seen, 1);
    assert.deepEqual(result.items, []);
    assert.equal(result.diagnostics.truncated, true);
  } finally {
    clock.mock.restore();
  }
});

test("all-occurrence mode still rejects real nonzero rg exits without a ranker", async (t) => {
  const root = await fixture(t, "needle needle\n");
  await assert.rejects(
    runRgSearch({
      root,
      patterns: ["needle"],
      matchAllOccurrences: true,
      rgOptions: { extraArgs: ["--zg-invalid-option"] },
    }),
    /failed with exit code 2/,
  );
});

test("ranked rg retains path, glob, ignore, hidden and modification-time scope", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-ranked-scope-");
  for (const directory of ["src", "docs", ".hidden", ".git", ".zvec-grep"])
    await mkdir(join(root, directory));
  for (const path of [
    "src/keep.ts",
    "src/skip.ts",
    "src/old.ts",
    "src/other.js",
    "docs/keep.ts",
    ".hidden/keep.ts",
    ".git/keep.ts",
    ".zvec-grep/keep.ts",
    "ignored.ts",
  ])
    await writeFile(join(root, path), "needle\n");
  await writeFile(join(root, ".ignore"), "ignored.ts\n");
  await utimes(join(root, "src/old.ts"), 1, 1);
  const seen = [];
  const scoped = await runRgSearch({
    root,
    patterns: ["needle"],
    paths: ["src"],
    excludePaths: ["src/skip.ts", "src/other.js"],
    modifiedAfter: 2_000,
    rankItem(item) {
      seen.push(item.file.relativePath);
      return 1;
    },
  });
  assert.deepEqual(seen, ["src/keep.ts"]);
  assert.deepEqual(
    scoped.items.map((item) => item.file.relativePath),
    seen,
  );

  const search = (extra = {}) =>
    runRgSearch({ root, patterns: ["needle"], rankItem: () => 1, ...extra });
  const globbed = await search({ paths: ["src"], globs: ["*.ts"] });
  assert.deepEqual(globbed.items.map((item) => item.file.relativePath).sort(), [
    "src/keep.ts",
    "src/old.ts",
    "src/skip.ts",
  ]);
  // Preserve the existing rg argument precedence: later positive globs can
  // override earlier excludePaths. Ranking does not redefine this behavior.
  const combinedOptions = {
    root,
    patterns: ["needle"],
    paths: ["src"],
    excludePaths: ["src/skip.ts"],
    globs: ["*.ts"],
    modifiedAfter: 2_000,
  };
  const legacyCombined = await runRgSearch(combinedOptions);
  const rankedCombined = await search(combinedOptions);
  assert.deepEqual(
    legacyCombined.items.map((item) => item.file.relativePath).sort(),
    ["src/keep.ts", "src/skip.ts"],
  );
  assert.deepEqual(
    rankedCombined.items.map((item) => item.file.relativePath).sort(),
    legacyCombined.items.map((item) => item.file.relativePath).sort(),
  );
  assert.deepEqual(
    rankedCombined.diagnostics.args,
    legacyCombined.diagnostics.args,
  );
  const ordinary = (await search()).items.map((item) => item.file.relativePath);
  assert.ok(!ordinary.includes("ignored.ts"));
  assert.ok(!ordinary.some((path) => path.startsWith(".")));
  const expanded = (await search({ hidden: true, noIgnore: true })).items.map(
    (item) => item.file.relativePath,
  );
  assert.ok(expanded.includes("ignored.ts"));
  assert.ok(expanded.includes(".hidden/keep.ts"));
  assert.ok(expanded.every((path) => !/^\.(git|zvec-grep)\//.test(path)));
  const missing = await search({ paths: ["missing"] });
  assert.deepEqual(missing.items, []);
  assert.deepEqual(missing.diagnostics.missingPaths, ["missing"]);
  assert.equal(missing.diagnostics.truncated, false);
});

test("scanLimit counts rejected raw matches and only truncates when exceeded", async (t) => {
  const root = await fixture(
    t,
    "needle 1\nneedle 2\nneedle 3\nneedle 4\nneedle 5\nneedle 6\n",
  );
  const search = async (scanLimit) => {
    const seen = [];
    const result = await runRgSearch({
      root,
      patterns: ["needle"],
      limit: 10,
      scanLimit,
      rankItem(item) {
        const number = Number(item.content.split(" ")[1]);
        seen.push(number);
        return number < 5 ? 0 : number;
      },
    });
    return { result, seen };
  };
  const bounded = await search(5);
  assert.deepEqual(bounded.seen, [1, 2, 3, 4, 5]);
  assert.deepEqual(
    bounded.result.items.map((item) => item.content),
    ["needle 5"],
  );
  assert.equal(bounded.result.diagnostics.truncated, true);
  const complete = await search(6);
  assert.deepEqual(complete.seen, [1, 2, 3, 4, 5, 6]);
  assert.deepEqual(
    complete.result.items.map((item) => item.content),
    ["needle 6", "needle 5"],
  );
  assert.equal(complete.result.diagnostics.truncated, false);
  const empty = await search(0);
  assert.deepEqual(empty.seen, []);
  assert.deepEqual(empty.result.items, []);
  assert.equal(empty.result.diagnostics.truncated, true);
});

test("scanLimit can bound an unranked search without changing match order", async (t) => {
  const root = await fixture(t, "needle first\nneedle second\nneedle third\n");
  const result = await runRgSearch({
    root,
    patterns: ["needle"],
    scanLimit: 2,
  });
  assert.deepEqual(
    result.items.map((item) => item.content),
    ["needle first", "needle second"],
  );
  assert.equal(result.diagnostics.truncated, true);
});

test(
  "timeout returns a truncated result and does not stop another rg invocation",
  { timeout: 5_000 },
  async (t) => {
    const root = await fixture(t, "needle first\nneedle second\n");
    const [expired, complete] = await Promise.all([
      runRgSearch({
        root,
        patterns: ["needle"],
        timeoutMs: 0,
        rankItem: () => 1,
      }),
      runRgSearch({
        root,
        patterns: ["needle"],
        timeoutMs: 4_000,
        rankItem: () => 1,
      }),
    ]);
    assert.deepEqual(expired.items, []);
    assert.equal(expired.diagnostics.truncated, true);
    assert.equal(complete.items.length, 2);
    assert.equal(complete.diagnostics.truncated, false);
  },
);

test("literal rg without new options still keeps the first matches and expands context", async (t) => {
  const root = await fixture(
    t,
    "before\nneedle first\nneedle second\nneedle strong\n",
  );
  const result = await runRgSearch({
    root,
    patterns: ["needle"],
    limit: 2,
    rgOptions: { beforeContext: 1 },
  });
  assert.deepEqual(
    result.items.map((item) => item.content),
    ["before\nneedle first", "needle first\nneedle second"],
  );
  assert.deepEqual(
    result.items.map((item) => item.rank),
    [1, 2],
  );
  assert.equal(result.diagnostics.truncated, true);
  const unlimited = await runRgSearch({ root, patterns: ["needle"] });
  assert.equal(unlimited.items.length, 3);
  assert.equal(unlimited.diagnostics.truncated, false);
});

test("bounded context skips oversized files while legacy context still expands", async (t) => {
  const root = await fixture(
    t,
    "before first\nneedle first\nafter first\n" +
      `${"padding ".repeat(128)}\n`.repeat(1_030) +
      "before second\nneedle second\nafter second\n",
  );
  const seen = [];
  const options = {
    root,
    patterns: ["needle"],
    rgOptions: { beforeContext: 1, afterContext: 1 },
  };
  const bounded = await runRgSearch({
    ...options,
    rankItem(item) {
      seen.push(item);
      return 1;
    },
  });
  assert.deepEqual(
    seen.map((item) => item.content),
    ["needle first", "needle second"],
  );
  assert.ok(seen.every((item) => item.excerptRange === undefined));
  assert.equal(bounded.diagnostics.truncated, true);
  const legacy = await runRgSearch(options);
  assert.deepEqual(
    legacy.items.map((item) => item.content),
    [
      "before first\nneedle first\nafter first",
      "before second\nneedle second\nafter second",
    ],
  );
  assert.equal(legacy.diagnostics.truncated, false);
});

test("an oversized JSON match stops bounded rg before parsing or ranking it", async (t) => {
  const root = await fixture(
    t,
    `needle retained\nneedle ${"x".repeat(270_000)}\nneedle later\n`,
  );
  const seen = [];
  const result = await runRgSearch({
    root,
    patterns: ["needle"],
    rankItem(item) {
      seen.push(item.content);
      return 1;
    },
  });
  assert.deepEqual(seen, ["needle retained"]);
  assert.deepEqual(
    result.items.map((item) => item.content),
    seen,
  );
  assert.equal(result.diagnostics.truncated, true);
});

test("the JSON bound applies per record, not to cumulative short-match output", async (t) => {
  const root = await fixture(t, `needle ${"x".repeat(100)}\n`.repeat(3_000));
  let seen = 0;
  const result = await runRgSearch({
    root,
    patterns: ["needle"],
    limit: 1,
    rankItem() {
      seen++;
      return 1;
    },
  });
  assert.equal(seen, 3_000);
  assert.equal(result.items.length, 1);
  assert.equal(result.diagnostics.truncated, true);
});

test("bounded stderr stays capped without converting an rg error to success", async (t) => {
  const root = await fixture(t, "needle\n");
  await assert.rejects(
    runRgSearch({
      root,
      patterns: ["needle"],
      rankItem: () => 1,
      rgOptions: { extraArgs: [`--zg-invalid-option-${"x".repeat(20_000)}`] },
    }),
    (error) => {
      assert.match(error.message, /failed with exit code 2/);
      assert.match(error.message, /\[stderr truncated\]/);
      assert.ok(error.message.length < 17_000);
      return true;
    },
  );
});

test("ranker failures reject cleanly and invalid internal budgets fail before scanning", async (t) => {
  const root = await fixture(t, "needle\n");
  const base = { root, patterns: ["needle"] };
  await assert.rejects(
    runRgSearch({
      ...base,
      rankItem: () => {
        throw new Error("ranker failed");
      },
    }),
    /ranker failed/,
  );
  await assert.rejects(runRgSearch({ ...base, scanLimit: -1 }), /scanLimit/);
  await assert.rejects(runRgSearch({ ...base, timeoutMs: NaN }), /timeoutMs/);
  assert.equal((await runRgSearch(base)).items.length, 1);
});
