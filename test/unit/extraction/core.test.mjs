import assert from "node:assert/strict";
import test from "node:test";
import { CodeExtractor } from "../../../dist/engine/extraction/code/extractor.js";
import { ImageExtractor } from "../../../dist/engine/extraction/image/extractor.js";
import { MarkdownExtractor } from "../../../dist/engine/extraction/markdown/extractor.js";
import { extract } from "../../../dist/engine/extraction/index.js";
import { TextExtractor } from "../../../dist/engine/extraction/text/extractor.js";

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
  const extractor = new TextExtractor();
  await assert.rejects(
    extractor.extract(textSource("value"), { maxChunkChars: 0 }),
    /chunk size/,
  );
  await assert.rejects(
    extractor.extract(textSource("value"), {
      maxChunkChars: 10,
      chunkOverlapChars: 10,
    }),
    /overlap/,
  );
  await assert.rejects(
    extractor.extract(textSource("value"), {
      maxChunkChars: 10,
      chunkOverlapChars: -1,
    }),
    /overlap/,
  );

  const chunks = await extractor.extract(
    textSource("alpha beta\ngamma delta\nepsilon zeta\n"),
    { maxChunkChars: 18, chunkOverlapChars: 6 },
  );
  assert.ok(chunks.length >= 2);
  assert.equal(chunks[0].id.length, 64);
  assert.equal(chunks[0].range.kind, "text");
  assert.equal(chunks[0].range.startLine, 1);

  const longLine = await extractor.extract(
    textSource("sentence, with punctuation and-more"),
    { maxChunkChars: 10, chunkOverlapChars: 0 },
  );
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

test("global extraction keeps concurrent chunk options isolated", async () => {
  const source = textSource(
    Array.from({ length: 20 }, (_, index) => `line ${index}`).join("\n"),
  );
  const [smallChunks, largeChunks] = await Promise.all([
    extract(source, { maxChunkChars: 24, chunkOverlapChars: 0 }),
    extract(source, { maxChunkChars: 240, chunkOverlapChars: 0 }),
  ]);

  assert.ok(smallChunks.length > largeChunks.length);
  assert.equal(
    smallChunks.every((fragment) => fragment.content.text.length <= 24),
    true,
  );
  assert.equal(
    largeChunks.every((fragment) => fragment.content.text.length <= 240),
    true,
  );
});

test("global extraction routes code, markdown, text, and image sources", async () => {
  const [code, markdown, text, image] = await Promise.all([
    extract(
      textSource("export function routed() { return true; }", {
        kind: "code",
        format: "typescript",
        relativePath: "fixture.ts",
      }),
    ),
    extract(
      textSource("# Routed\nBody", {
        kind: "text",
        format: "markdown",
        relativePath: "README.md",
      }),
    ),
    extract(
      textSource('{"routed":true}', {
        kind: "data",
        format: "json",
        relativePath: "fixture.json",
      }),
    ),
    extract({
      kind: "image",
      data: new Uint8Array([1, 2, 3]),
      format: "png",
      file: file({ kind: "image", format: "png", relativePath: "fixture.png" }),
    }),
  ]);

  assert.equal(code[0].metadata.kind, "code");
  assert.equal(code[0].metadata.symbolName, "routed");
  assert.equal(markdown[0].metadata.kind, "markdown");
  assert.equal(markdown[0].metadata.heading, "Routed");
  assert.equal(text[0].metadata, undefined);
  assert.equal(text[0].content.text, '{"routed":true}');
  assert.equal(image[0].content.kind, "image");
  assert.equal(image[0].content.format, "png");
});

test("image extractor validates data", async () => {
  const source = {
    kind: "image",
    data: new Uint8Array([1, 2, 3]),
    format: "png",
    file: file({ kind: "image", format: "png" }),
  };
  const extractor = new ImageExtractor();
  const [fragment] = await extractor.extract(source);
  assert.equal(fragment.range.kind, "file");
  assert.equal(fragment.content.data, source.data);

  await assert.rejects(
    extractor.extract({ ...source, data: new Uint8Array() }),
    /non-empty image data/,
  );
  await assert.rejects(extractor.extract(textSource("text")), /non-image/);
});

test("markdown extractor handles heading styles, fences, hierarchy, and windows", async () => {
  const extractor = new MarkdownExtractor();
  const optionSource = textSource("# Heading", {
    kind: "text",
    format: "markdown",
  });
  await assert.rejects(
    extractor.extract(optionSource, { maxChunkChars: 0 }),
    /chunk size/,
  );
  await assert.rejects(
    extractor.extract(optionSource, {
      maxChunkChars: 10,
      chunkOverlapChars: 10,
    }),
    /overlap/,
  );
  await assert.rejects(
    extractor.extract(optionSource, {
      maxChunkChars: 10,
      chunkOverlapChars: -1,
    }),
    /overlap/,
  );
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
  const fragments = await extractor.extract(source, {
    maxChunkChars: 48,
    chunkOverlapChars: 8,
  });
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
  const plainMarkdown = await extractor.extract(
    textSource("plain markdown", { kind: "text", format: "markdown" }),
  );
  assert.equal(plainMarkdown.length, 1);
  assert.equal(plainMarkdown[0].content.text, "plain markdown");
  assert.equal(plainMarkdown[0].metadata, undefined);
});

test("code extractor parses TypeScript and script blocks", async () => {
  const extractor = new CodeExtractor();
  const optionSource = textSource("export const value = 1;", {
    kind: "code",
    format: "typescript",
  });
  await assert.rejects(
    extractor.extract(optionSource, { maxChunkChars: 0 }),
    /chunk size/,
  );
  await assert.rejects(
    extractor.extract(optionSource, {
      maxChunkChars: 20,
      chunkOverlapChars: 20,
    }),
    /overlap/,
  );
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
  const fragments = await extractor.extract(typescript, {
    maxChunkChars: 120,
    chunkOverlapChars: 20,
  });
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
  const unknownCode = await extractor.extract(
    textSource("value", { kind: "code", format: "unknown" }),
  );
  assert.equal(unknownCode.length, 1);
  assert.equal(unknownCode[0].content.text, "value");
  assert.equal(unknownCode[0].metadata, undefined);

  const plainCode = await extractor.extract(
    textSource("// no declarations", {
      kind: "code",
      format: "typescript",
      relativePath: "plain.ts",
    }),
  );
  assert.equal(plainCode.length, 1);
  assert.equal(plainCode[0].content.text, "// no declarations");
  assert.equal(plainCode[0].metadata, undefined);

  const defaults = await extract(typescript);
  assert.ok(defaults.length > 0);
});

test("code extractor applies character limits to oversized single lines", async () => {
  const maxChunkChars = 48;
  const extractor = new CodeExtractor();
  const source = textSource(
    `export function f() { return "${"value".repeat(40)}"; }`,
    { kind: "code", format: "typescript", relativePath: "fixture.ts" },
  );

  const fragments = (
    await extractor.extract(source, {
      maxChunkChars,
      chunkOverlapChars: 6,
    })
  ).filter((fragment) => fragment.metadata?.symbolName === "f");
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
  const extractor = new CodeExtractor();
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
    const fragments = await extractor.extract(source, {
      maxChunkChars: 500,
      chunkOverlapChars: 50,
    });
    assert.ok(fragments.length > 0, format);
    assert.ok(
      fragments.some((item) => item.metadata?.symbolName === expectedName),
      `${format} should expose ${expectedName}`,
    );
  }
});
