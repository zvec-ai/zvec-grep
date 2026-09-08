import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { runRgSearch } from "../../dist/engine/service/lexical.js";
import { enrichLexicalItemsWithStructure } from "../../dist/engine/service/structure-enrichment.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

async function fixture(t, source) {
  const root = await createTemporaryDirectory(t, "zg-keyword-structure-");
  const path = join(root, "source.ts");
  await writeFile(path, source);
  return {
    root,
    path,
    async search(patterns = ["retry", "timeout"]) {
      return runRgSearch({
        root,
        paths: [path],
        patterns,
        limit: 100,
        rgOptions: { ignoreCase: true, beforeContext: 2, afterContext: 2 },
      });
    },
  };
}

function strict(root, items, fileLimit = 25) {
  return enrichLexicalItemsWithStructure(
    root,
    items,
    fileLimit,
    1024 * 1024,
    true,
  );
}

test("strict keyword structure separates adjacent functions using real rg windows", async (t) => {
  const state = await fixture(
    t,
    "export function retryOnly() { retry(); }\nexport function timeoutOnly() { timeout(); }\n",
  );
  const recalled = await state.search();
  assert.equal(recalled.items.length, 2);
  assert.ok(
    recalled.items.every(
      (item) =>
        item.content.includes("retryOnly") &&
        item.content.includes("timeoutOnly"),
    ),
    "the actual expanded rg windows initially contain both functions",
  );
  const result = await strict(state.root, recalled.items);
  assert.equal(result.diagnostics.enrichedItems, 2);
  assert.deepEqual(
    result.items.map((item) => item.container.metadata.symbolName),
    ["retryOnly", "timeoutOnly"],
  );
  for (const item of result.items) {
    const name = item.container.metadata.symbolName;
    assert.ok(item.content.includes(name));
    assert.ok(
      !item.content.includes(
        name === "retryOnly" ? "timeoutOnly" : "retryOnly",
      ),
    );
  }

  const legacy = await enrichLexicalItemsWithStructure(
    state.root,
    recalled.items,
  );
  assert.deepEqual(
    legacy.items.map((item) => item.content),
    recalled.items.map((item) => item.content),
    "legacy literal enrichment does not clip source windows",
  );
});

test("strict keyword structure chooses the actual owner of same-line matches", async (t) => {
  const retry = "export function retryOnly() { retry(); }";
  const timeout = "export function timeoutOnly() { timeout(); }";
  const state = await fixture(t, `${retry} ${timeout}\n`);
  for (const [name, expected] of [
    ["retryOnly", retry],
    ["timeoutOnly", timeout],
  ]) {
    const recalled = await state.search([name]);
    assert.equal(recalled.items.length, 1);
    assert.equal(recalled.items[0].content, `${retry} ${timeout}`);
    const result = await strict(state.root, recalled.items);
    assert.equal(result.items[0].container.metadata.symbolName, name);
    assert.equal(result.items[0].content, expected);
    assert.equal(result.items[0].range.startLine, 1);
    assert.equal(result.items[0].range.endLine, 1);
  }
});

test("strict keyword structure retains a real multi-line window within one function", async (t) => {
  const source = [
    "export function scheduleAttempt() {",
    "  retry();",
    "  timeout();",
    "  return 1;",
    "}",
    "",
  ].join("\n");
  const state = await fixture(t, source);
  const result = await strict(state.root, (await state.search()).items);
  assert.equal(result.items.length, 2);
  assert.ok(
    result.items.every(
      (item) => item.container.metadata.symbolName === "scheduleAttempt",
    ),
  );
  assert.ok(
    result.items.every(
      (item) =>
        item.content.includes("retry();") &&
        item.content.includes("timeout();"),
    ),
  );
  assert.ok(result.items.every((item) => source.includes(item.content)));
});

test("strict source positions distinguish UTF-8 rg bytes, UTF-16 columns and CRLF file offsets", async (t) => {
  const prefix = "// café 😀\r\n";
  const sameLinePrefix = 'const café = "😀"; ';
  const retry = "export function retryOnly() { retry(); }";
  const timeout = "export function timeoutOnly() { timeout(); }";
  const line = `${sameLinePrefix}${retry} ${timeout}`;
  const source = `${prefix}${line}\r\n`;
  const state = await fixture(t, source);
  const recalled = await state.search(["timeoutOnly"]);
  const raw = recalled.items[0];
  assert.equal(raw.excerptRange.startLine, 2);
  assert.equal(raw.excerptRange.startOffset, line.indexOf("timeoutOnly"));
  assert.notEqual(
    raw.excerptRange.startOffset,
    Buffer.byteLength(line.slice(0, line.indexOf("timeoutOnly"))),
  );
  const result = await strict(state.root, recalled.items);
  const item = result.items[0];
  assert.equal(item.container.metadata.symbolName, "timeoutOnly");
  assert.equal(item.container.range.startOffset, source.indexOf(timeout));
  assert.equal(
    item.container.range.endOffset,
    source.indexOf(timeout) + timeout.length,
  );
  assert.equal(item.content, timeout);
  assert.equal(item.range.startLine, 2);
  assert.equal(item.range.endLine, 2);
  assert.equal(item.range.startOffset, line.indexOf(timeout));
  assert.equal(item.range.endOffset, line.length);
  assert.deepEqual(item.excerptRange, raw.excerptRange);

  const multiline = [
    "// café 😀",
    "export function scheduleAttempt() {",
    '  const café = "😀";',
    "  retry();",
    "  timeout();",
    "}",
    "",
  ].join("\r\n");
  await writeFile(state.path, multiline);
  const multiple = await strict(state.root, (await state.search()).items);
  assert.ok(multiple.items.length > 0);
  for (const current of multiple.items) {
    assert.equal(current.container.metadata.symbolName, "scheduleAttempt");
    assert.ok(current.content.includes("retry();"));
    assert.ok(current.content.includes("timeout();"));
    assert.ok(!current.content.includes("\r"));
    assert.ok(multiline.replace(/\r\n/g, "\n").includes(current.content));
  }
});

test("strict keyword enrichment never attaches a newly parsed owner to an old rg window", async (t) => {
  const state = await fixture(
    t,
    "export function beforeEdit() { retry(); timeout(); }\n",
  );
  const recalled = await state.search();
  await writeFile(
    state.path,
    "export function afterEdit() { retry(); timeout(); }\n",
  );
  const result = await strict(state.root, recalled.items);
  assert.equal(result.diagnostics.enrichedItems, 0);
  assert.ok(result.items.length > 0);
  for (const item of result.items) {
    assert.equal(item.container, undefined);
    assert.equal(item.metadata, undefined);
    assert.equal(
      item.content,
      "",
      "a caller cannot reuse the old raw match line",
    );
    assert.equal(item.status, "possibly_stale");
  }
});

test("strict keyword enrichment leaves over-budget or unstructured code without a fabricated owner", async (t) => {
  const state = await fixture(
    t,
    "export function retryOnly() {}\nexport function timeoutOnly() {}\n",
  );
  const recalled = await state.search();
  const limited = await strict(state.root, recalled.items, 0);
  assert.equal(limited.diagnostics.truncated, true);
  assert.ok(limited.items.every((item) => item.container === undefined));
  assert.deepEqual(
    limited.items.map((item) => item.content),
    recalled.items.map((item) => item.content),
  );

  await writeFile(state.path, "// retry\n// timeout\n");
  const unstructured = await strict(state.root, (await state.search()).items);
  assert.equal(unstructured.diagnostics.parsedFiles, 0);
  assert.ok(unstructured.items.every((item) => item.container === undefined));
});
