import assert from "node:assert/strict";
import test from "node:test";
import {
  currentCodeExtractionVersion,
  extractForIndexing,
} from "../../../dist/engine/extraction/index.js";

function source(format, text) {
  return {
    kind: "text",
    text,
    file: {
      id: "js-binding-fixture",
      absolutePath: "/repo/bindings.js",
      relativePath: "bindings.js",
      rootPath: "/repo",
      sizeBytes: Buffer.byteLength(text),
      lastModifiedTime: 1,
      kind: "code",
      format,
    },
  };
}

function named(fragments, name) {
  return fragments.find(
    ({ fragment }) => fragment.metadata?.symbolName === name,
  )?.fragment;
}

test("JavaScript and TypeScript function assignments use the static binding name and receiver", async () => {
  for (const format of ["javascript", "jsx", "typescript", "tsx"]) {
    const input = source(
      format,
      [
        'const example = "sendFile(path)";',
        "/** Copy a file to the response. */",
        "res.sendFile = function implementation(path) { return path; };",
        "module.exports.sendResource = async (path) => path;",
        'res["clearCookie"] = function(value) { return value; };',
        "retryHandler = (function(value) { return value; });",
        "res.streamChunks = function*() { yield chunk; };",
        "const parenthesizedHandler = ((value) => value);",
      ].join("\n"),
    );
    const fragments = await extractForIndexing(input);
    const definition = named(fragments, "sendFile");
    assert.ok(definition, format + ": missing property-assigned definition");
    assert.equal(definition.metadata.symbolType, "function");
    assert.equal(definition.metadata.scope, "res");
    assert.equal(
      definition.metadata.signature,
      "res.sendFile = function implementation(path)",
    );
    assert.equal(definition.metadata.doc, "Copy a file to the response.");
    assert.equal(definition.range.startLine, 3);
    assert.equal(
      named(fragments, "sendResource").metadata.scope,
      "module.exports",
    );
    assert.ok(
      named(fragments, "sendResource").metadata.modifiers.includes("async"),
    );
    assert.equal(named(fragments, "clearCookie").metadata.scope, "res");
    assert.equal(named(fragments, "retryHandler").metadata.scope, null);
    for (const name of [
      "sendFile",
      "sendResource",
      "clearCookie",
      "retryHandler",
      "streamChunks",
      "parenthesizedHandler",
    ]) {
      const fragment = named(fragments, name);
      assert.equal(
        fragment.content.text,
        input.text.slice(fragment.range.startOffset, fragment.range.endOffset),
      );
    }
    assert.deepEqual(await extractForIndexing(input), fragments);
  }
});

test("component script bindings preserve source positions and use the JS/TS extraction revision", async () => {
  for (const format of ["vue", "svelte"]) {
    const input = source(
      format,
      '<template>sendFile example</template>\n<script lang="ts">\nres.sendFile = (path) => path;\n</script>',
    );
    const fragments = await extractForIndexing(input);
    const definition = named(fragments, "sendFile");
    assert.equal(definition.range.startLine, 3);
    assert.equal(definition.metadata.scope, "res");
    assert.equal(
      definition.content.text,
      input.text.slice(
        definition.range.startOffset,
        definition.range.endOffset,
      ),
    );
  }
  for (const format of [
    "javascript",
    "jsx",
    "typescript",
    "tsx",
    "vue",
    "svelte",
  ])
    assert.equal(currentCodeExtractionVersion(format), 2);
  for (const format of ["python", "go", "rust", "java", "c", "cpp", "ruby"])
    assert.equal(currentCodeExtractionVersion(format), 1);
});

test("dynamic properties and callback-taking calls are not fabricated function definitions", async () => {
  const input = source(
    "javascript",
    [
      "api[methodName] = function(value) { return value; };",
      "api.handlers = values.map(value => value);",
      "api.sendFile = existingHandler;",
      "api.isReady = true;",
      'api["escaped\\u004eame"] = function(value) { return value; };',
    ].join("\n"),
  );
  const fragments = await extractForIndexing(input);
  for (const name of [
    "methodName",
    "handlers",
    "sendFile",
    "isReady",
    "escaped\\u004eame",
  ])
    assert.equal(named(fragments, name), undefined, name);
  assert.ok(
    fragments.some(({ fragment }) =>
      fragment.content.text.includes("existingHandler"),
    ),
    "unrecognized bindings remain searchable source",
  );
});

test("large assigned functions retain grouped, bounded source windows and binding metadata", async () => {
  const input = source(
    "typescript",
    "service.processPayload = function(value) {\n" +
      Array.from({ length: 30 }, (_, i) => `  value = step${i}(value);`).join(
        "\n",
      ) +
      "\n  return value;\n};",
  );
  const fragments = await extractForIndexing(input, {
    maxChunkChars: 180,
    chunkOverlapChars: 24,
  });
  const group = fragments.find(
    ({ fragment }) =>
      fragment.metadata?.symbolName === "processPayload" &&
      fragment.group === fragment.id,
  )?.fragment;
  assert.ok(group);
  const windows = fragments.filter(
    ({ fragment }) => fragment.group === group.id && fragment.id !== group.id,
  );
  assert.ok(windows.length > 1);
  for (const { fragment } of windows) {
    assert.equal(fragment.metadata.scope, "service");
    assert.ok(fragment.content.text.length <= 180);
    assert.equal(
      fragment.content.text,
      input.text.slice(fragment.range.startOffset, fragment.range.endOffset),
    );
  }
});
