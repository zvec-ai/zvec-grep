import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { enrichLexicalItemsWithStructure } from "../../dist/engine/service/structure-enrichment.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

function lexicalItem(root, relativePath, line, content) {
  return {
    kind: "lexical_match",
    rank: 0,
    file: {
      absolutePath: join(root, relativePath),
      relativePath,
      rootPath: root,
    },
    range: {
      kind: "text",
      startLine: line,
      endLine: line,
      startOffset: 0,
      endOffset: content.length,
    },
    content,
    status: "fresh",
    matchedBy: "lexical",
  };
}

test("structure enrichment ignores plain-text extraction fallbacks", async (t) => {
  const root = await createTemporaryDirectory(t, "zg-structure-");
  const fixtures = [
    ["unsupported.rb", 'puts "hello"\n'],
    ["plain.ts", "// no declarations\n"],
    ["plain.md", "markdown without headings\n"],
    ["structured.ts", 'export function greet() {\n  return "hello";\n}\n'],
  ];

  for (const [relativePath, content] of fixtures) {
    await writeFile(join(root, relativePath), content);
  }

  const result = await enrichLexicalItemsWithStructure(root, [
    lexicalItem(root, "unsupported.rb", 1, 'puts "hello"'),
    lexicalItem(root, "plain.ts", 1, "// no declarations"),
    lexicalItem(root, "plain.md", 1, "markdown without headings"),
    lexicalItem(root, "structured.ts", 2, '  return "hello";'),
  ]);
  const items = new Map(
    result.items.map((item) => [item.file.relativePath, item]),
  );

  assert.equal(items.get("unsupported.rb").container, undefined);
  assert.equal(items.get("plain.ts").container, undefined);
  assert.equal(items.get("plain.md").container, undefined);
  assert.equal(items.get("structured.ts").container.metadata.kind, "code");
  assert.deepEqual(result.diagnostics, {
    source: "structural_extraction",
    fileLimit: 100,
    matchedFiles: 4,
    parsedFiles: 1,
    enrichedFiles: 1,
    enrichedItems: 1,
    skippedFiles: 3,
    truncated: false,
  });
});
