import assert from "node:assert/strict";
import test from "node:test";
import { extractForIndexing } from "../../../dist/engine/extraction/index.js";

function source(format, text) {
  return {
    kind: "text",
    text,
    file: {
      id: "coverage-fixture",
      absolutePath: `/repo/fixture.${format}`,
      relativePath: `fixture.${format}`,
      rootPath: "/repo",
      sizeBytes: Buffer.byteLength(text),
      lastModifiedTime: 1,
      kind: "code",
      format,
    },
  };
}

function sourceFragments(input, prepared) {
  return prepared
    .map(({ fragment }) => fragment)
    .filter(
      (fragment) =>
        fragment.range.kind === "text" &&
        fragment.content.kind === "text" &&
        fragment.content.text ===
          input.text.slice(
            fragment.range.startOffset,
            fragment.range.endOffset,
          ),
    );
}

function assertCoverage(input, prepared) {
  const fragments = sourceFragments(input, prepared);
  for (let offset = 0; offset < input.text.length; offset++) {
    if (!/[\p{L}\p{N}_$]/u.test(input.text[offset])) continue;
    assert.ok(
      fragments.some(
        (fragment) =>
          fragment.range.startOffset <= offset &&
          fragment.range.endOffset > offset,
      ),
      `missing source at offset ${offset}: ${input.text.slice(offset, offset + 45)}`,
    );
  }
  const ids = prepared.map(({ fragment }) => fragment.id);
  assert.equal(new Set(ids).size, ids.length);
  for (const { fragment } of prepared) {
    if (fragment.group) assert.ok(ids.includes(fragment.group));
  }
}

test("recognizing a function must not remove top-level declarations from any supported language", async () => {
  for (const fixture of [
    [
      "typescript",
      "const RETRY_DELAYS = [100, 200];",
      "function retry() { return RETRY_DELAYS; }",
    ],
    [
      "python",
      "RETRY_DELAYS = [100, 200]",
      "def retry():\n    return RETRY_DELAYS",
    ],
    [
      "go",
      "package retry\nvar RetryDelays = []int{100, 200}",
      "func Retry() []int { return RetryDelays }",
    ],
    [
      "rust",
      "const RETRY_DELAYS: [u32; 2] = [100, 200];",
      "fn retry() -> [u32; 2] { RETRY_DELAYS }",
    ],
  ]) {
    const [format, declaration, fn] = fixture;
    const input = source(format, `${declaration}\n${fn}`);
    const prepared = await extractForIndexing(input);
    assertCoverage(input, prepared);
    assert.ok(
      sourceFragments(input, prepared).some((fragment) =>
        fragment.content.text.includes("100, 200"),
      ),
      format,
    );
    assert.ok(
      prepared.some(
        ({ fragment }) => fragment.metadata?.symbolType === "function",
      ),
    );
    assert.deepEqual(await extractForIndexing(input), prepared);
  }
});

test("gap fragments preserve CRLF, Unicode, source offsets and bounded chunks alongside outlines", async () => {
  const input = source(
    "typescript",
    [
      "/** Module startup configuration. */",
      'import { configure } from "runtime";',
      `const greeting = "${"中文😀".repeat(40)}";`,
      "export class Handler {",
      "  first() { return configure(greeting); }",
      "  second() { return this.first(); }",
      "}",
      "configure({ handler: new Handler() });",
    ].join("\r\n"),
  );
  const prepared = await extractForIndexing(input, {
    maxChunkChars: 80,
    chunkOverlapChars: 12,
  });
  assertCoverage(input, prepared);
  assert.ok(prepared.some(({ fragment }) => fragment.group === fragment.id));
  const plain = prepared.filter(({ fragment }) => !fragment.metadata);
  assert.ok(plain.length > 1);
  for (const { fragment } of plain) {
    assert.ok(fragment.content.text.length <= 80);
    assert.doesNotMatch(fragment.content.text, /\p{Surrogate}/u);
    assert.equal(
      fragment.range.startLine,
      input.text.slice(0, fragment.range.startOffset).split("\n").length,
    );
    assert.equal(
      fragment.range.endLine,
      input.text.slice(0, fragment.range.endOffset).split("\n").length,
    );
  }
});

test("component extraction retains template, module data and remapped script symbols", async () => {
  const input = source(
    "svelte",
    [
      "<h1>Connection retry settings</h1>",
      '<script lang="ts">',
      "const attempts = 3;",
      "export function retry() { return attempts; }",
      "</script>",
      "<p>Try the connection again.</p>",
      "<script>export function stop() { return false; }</script>",
    ].join("\n"),
  );
  const prepared = await extractForIndexing(input, {
    maxChunkChars: 120,
    chunkOverlapChars: 18,
  });
  assertCoverage(input, prepared);
  assert.equal(
    prepared.find(({ fragment }) => fragment.metadata?.symbolName === "retry")
      .fragment.range.startLine,
    4,
  );
  assert.equal(
    prepared.find(({ fragment }) => fragment.metadata?.symbolName === "stop")
      .fragment.range.startLine,
    7,
  );
});

test("declaration wrappers stay with their symbols instead of becoming standalone vector noise", async () => {
  for (const format of ["javascript", "typescript"]) {
    const input = source(
      format,
      [
        "export function first() { return 1; }",
        "export default function second() { return 2; }",
        "export const third = () => 3;",
        "const fourth = () => 4;",
      ].join("\n"),
    );
    const prepared = await extractForIndexing(input);
    assertCoverage(input, prepared);
    assert.equal(
      prepared.length,
      4,
      "export/const alone must not consume embedding or recall slots",
    );
    assert.deepEqual(
      prepared.map(({ fragment }) => fragment.metadata.symbolName),
      ["first", "second", "third", "fourth"],
    );
    assert.ok(
      sourceFragments(input, prepared).every((fragment) =>
        /^(export|const) /.test(fragment.content.text),
      ),
    );
  }
});
