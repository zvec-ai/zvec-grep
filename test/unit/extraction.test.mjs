import assert from "node:assert/strict";
import test from "node:test";
import { CodeExtractor } from "../../dist/engine/extraction/code/extractor.js";
import { MarkdownExtractor } from "../../dist/engine/extraction/markdown/extractor.js";
import {
  ExtractorRegistry,
  ImageExtractor,
  TextExtractor,
  extractFragments,
} from "../../dist/engine/extraction/index.js";

function file(overrides = {}) {
  return {
    id: "file-1",
    collectionId: "collection-1",
    absolutePath: "/repo/fixture.txt",
    relativePath: "fixture.txt",
    rootPath: "/repo",
    sizeBytes: 10,
    lastModifiedTime: 1,
    kind: "text",
    format: "text",
    ...overrides,
  };
}

function textSource(text, overrides = {}) {
  return { kind: "text", text, file: file(overrides) };
}

test("text extractor validates options, sources, chunks, and overlap", async () => {
  assert.throws(() => new TextExtractor({ maxChunkChars: 0 }), /chunk size/);
  assert.throws(
    () => new TextExtractor({ maxChunkChars: 10, chunkOverlapChars: 10 }),
    /overlap/,
  );
  assert.throws(
    () => new TextExtractor({ maxChunkChars: 10, chunkOverlapChars: -1 }),
    /overlap/,
  );

  const extractor = new TextExtractor({
    maxChunkChars: 18,
    chunkOverlapChars: 6,
  });
  assert.equal(extractor.supports(textSource("hello")), true);
  assert.equal(
    extractor.supports({
      kind: "image",
      data: new Uint8Array([1]),
      format: "png",
      file: file({ kind: "image", format: "png" }),
    }),
    false,
  );

  const chunks = await extractor.extract(
    textSource("alpha beta\ngamma delta\nepsilon zeta\n"),
  );
  assert.ok(chunks.length >= 2);
  assert.equal(chunks[0].id.length, 64);
  assert.equal(chunks[0].range.kind, "text");
  assert.equal(chunks[0].range.startLine, 1);

  const longLine = await new TextExtractor({
    maxChunkChars: 10,
    chunkOverlapChars: 0,
  }).extract(textSource("sentence, with punctuation and-more"));
  assert.ok(longLine.length >= 3);
  assert.equal(
    longLine.every((item) => item.content.text.length <= 10),
    true,
  );
  assert.deepEqual(await extractor.extract(textSource(" \n\t")), []);

  await assert.rejects(
    extractor.extract(textSource("value", { id: "" })),
    /non-empty file id/,
  );
  await assert.rejects(
    extractor.extract(textSource("value", { absolutePath: "" })),
    /absolute file path/,
  );
  await assert.rejects(
    extractor.extract(textSource("value", { relativePath: "" })),
    /relative file path/,
  );
  await assert.rejects(
    extractor.extract({
      kind: "image",
      data: new Uint8Array([1]),
      format: "png",
      file: file({ kind: "image", format: "png" }),
    }),
    /non-text source/,
  );
});

test("image extractor validates data and registry fallback behavior", async () => {
  const source = {
    kind: "image",
    data: new Uint8Array([1, 2, 3]),
    format: "png",
    file: file({ kind: "image", format: "png" }),
  };
  const extractor = new ImageExtractor();
  assert.equal(extractor.supports(source), true);
  const [fragment] = await extractor.extract(source);
  assert.equal(fragment.range.kind, "file");
  assert.equal(fragment.content.data, source.data);

  await assert.rejects(
    extractor.extract({ ...source, data: new Uint8Array() }),
    /non-empty image data/,
  );
  await assert.rejects(extractor.extract(textSource("text")), /non-image/);
  assert.throws(() => new ExtractorRegistry([]), /at least one extractor/);

  const empty = { supports: () => true, extract: async () => [] };
  const success = {
    supports: () => true,
    extract: async () => [
      {
        id: "custom",
        fileId: "file-1",
        range: { kind: "file" },
        content: { kind: "text", text: "custom" },
      },
    ],
  };
  const registry = new ExtractorRegistry([empty, success]);
  assert.equal(registry.resolve(textSource("value")), empty);
  assert.equal((await registry.extract(textSource("value")))[0].id, "custom");
  assert.deepEqual(
    await new ExtractorRegistry([empty]).extract(textSource("value")),
    [],
  );
  const unsupported = new ExtractorRegistry([
    { supports: () => false, extract: async () => [] },
  ]);
  assert.throws(() => unsupported.resolve(textSource("value")), /supports/);
  await assert.rejects(unsupported.extract(textSource("value")), /supports/);
});

test("markdown extractor handles heading styles, fences, hierarchy, and windows", async () => {
  assert.throws(
    () => new MarkdownExtractor({ maxChunkChars: 0 }),
    /chunk size/,
  );
  assert.throws(
    () => new MarkdownExtractor({ maxChunkChars: 10, chunkOverlapChars: 10 }),
    /overlap/,
  );
  assert.throws(
    () => new MarkdownExtractor({ maxChunkChars: 10, chunkOverlapChars: -1 }),
    /overlap/,
  );
  const extractor = new MarkdownExtractor({
    maxChunkChars: 48,
    chunkOverlapChars: 8,
  });
  const source = textSource(
    [
      "preface",
      "",
      "# Parent #",
      "intro paragraph",
      "```md",
      "# Not a heading",
      "```",
      "## Child",
      "- item one",
      "- item two with enough text to force another window",
      "Setext child",
      "------------",
      "body",
    ].join("\n"),
    { kind: "text", format: "markdown", relativePath: "README.md" },
  );
  assert.equal(extractor.supports(source), true);
  const fragments = await extractor.extract(source);
  assert.ok(fragments.length >= 4);
  assert.ok(fragments.some((item) => item.metadata?.heading === "Parent"));
  assert.ok(
    fragments.some(
      (item) =>
        item.metadata?.heading === "Child" && item.metadata?.scope === "Parent",
    ),
  );
  assert.equal(
    fragments.some((item) => item.metadata?.heading === "Not a heading"),
    false,
  );
  assert.ok(fragments.some((item) => item.group));

  await assert.rejects(
    extractor.extract({ ...source, file: { ...source.file, id: "" } }),
    /non-empty file id/,
  );

  assert.deepEqual(await extractor.extract(textSource("plain text")), []);
  assert.deepEqual(
    await extractor.extract(
      textSource("plain markdown", { kind: "text", format: "markdown" }),
    ),
    [],
  );
});

test("code extractor parses TypeScript and script blocks", async () => {
  assert.throws(() => new CodeExtractor({ maxChunkChars: 0 }), /chunk size/);
  assert.throws(
    () => new CodeExtractor({ maxChunkChars: 20, chunkOverlapChars: 20 }),
    /overlap/,
  );
  const extractor = new CodeExtractor({
    maxChunkChars: 120,
    chunkOverlapChars: 20,
  });
  const typescript = textSource(
    [
      "/** Computes a value. */",
      "export async function computeValue(input: number): Promise<number> {",
      "  return input + 1;",
      "}",
      "export const handler = () => computeValue(1);",
      "class Example { private value = 1; }",
    ].join("\n"),
    { kind: "code", format: "typescript", relativePath: "fixture.ts" },
  );
  assert.equal(extractor.supports(typescript), true);
  const fragments = await extractor.extract(typescript);
  assert.ok(fragments.length >= 2);
  assert.ok(
    fragments.some((item) => item.metadata?.symbolName === "computeValue"),
  );
  assert.ok(fragments.some((item) => item.metadata?.symbolName === "handler"));
  await assert.rejects(
    extractor.extract({
      ...typescript,
      file: { ...typescript.file, relativePath: "" },
    }),
    /relative file path/,
  );

  const vue = textSource(
    '<template><div /></template>\n<script setup lang="ts">\nexport const insideBlock = () => 1;\n</script>',
    { kind: "code", format: "vue", relativePath: "fixture.vue" },
  );
  const scriptFragments = await extractor.extract(vue);
  assert.ok(
    scriptFragments.some((item) => item.metadata?.symbolName === "insideBlock"),
  );
  assert.deepEqual(
    await extractor.extract(
      textSource("value", { kind: "code", format: "unknown" }),
    ),
    [],
  );

  const defaults = await extractFragments(typescript);
  assert.ok(defaults.length > 0);
});

test("code extractor applies character limits to oversized single lines", async () => {
  const maxChunkChars = 48;
  const extractor = new CodeExtractor({
    maxChunkChars,
    chunkOverlapChars: 6,
  });
  const source = textSource(
    `export function f() { return "${"value".repeat(40)}"; }`,
    { kind: "code", format: "typescript", relativePath: "fixture.ts" },
  );

  const fragments = (await extractor.extract(source)).filter(
    (fragment) => fragment.metadata?.symbolName === "f",
  );
  assert.ok(fragments.length > 2);

  for (const fragment of fragments) {
    assert.ok(fragment.content.text.length <= maxChunkChars);
  }
  for (const fragment of fragments.slice(1)) {
    assert.equal(
      fragment.content.text,
      source.text.slice(fragment.range.startOffset, fragment.range.endOffset),
    );
  }
});

test("code extractor supports the bundled language grammar and adapter matrix", async () => {
  const extractor = new CodeExtractor({
    maxChunkChars: 500,
    chunkOverlapChars: 50,
  });
  const fixtures = [
    ["c", "int add(int a, int b) { return a + b; }", "add"],
    ["cpp", "class Widget { public: int value() { return 1; } };", "Widget"],
    ["go", "package main\nfunc Add(a int, b int) int { return a + b }", "Add"],
    ["java", "class Widget { public int value() { return 1; } }", "Widget"],
    [
      "python",
      "class Widget:\n    def value(self):\n        return 1",
      "Widget",
    ],
    [
      "rust",
      "pub struct Widget { value: i32 }\nimpl Widget { pub fn value(&self) -> i32 { self.value } }",
      "Widget",
    ],
    [
      "javascript",
      "/** docs */\nexport class Widget { static value() { return 1; } }",
      "Widget",
    ],
  ];

  for (const [format, sourceText, expectedName] of fixtures) {
    const source = textSource(sourceText, {
      kind: "code",
      format,
      relativePath: `fixture.${format}`,
    });
    assert.equal(extractor.supports(source), true, format);
    const fragments = await extractor.extract(source);
    assert.ok(fragments.length > 0, format);
    assert.ok(
      fragments.some((item) => item.metadata?.symbolName === expectedName),
      `${format} should expose ${expectedName}`,
    );
  }
});
