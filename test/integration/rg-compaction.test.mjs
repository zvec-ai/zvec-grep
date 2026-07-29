import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { formatAgentContextResult } from "../../dist/cli/format/context.js";
import { createZvecGrep } from "../../dist/index.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

async function createRgService(t, prefix) {
  const temporaryDirectory = await createTemporaryDirectory(t, prefix);
  const root = join(temporaryDirectory, "repo");
  const home = join(temporaryDirectory, "home");
  await mkdir(root, { recursive: true });
  const service = await createZvecGrep({ root, home });
  t.after(() => service.close());
  return { root, service };
}

test("rg canonicalizes exact build mirrors and groups source occurrences by symbol", async (t) => {
  const { root, service } = await createRgService(t, "zvec-rg-symbol-mirror-");
  const source = [
    "export class PreparedRequest {",
    "  prepareContentLength() {",
    '    this.headers["Content-Length"] = "one";',
    '    this.headers["Content-Length"] = "two";',
    '    return "Content-Length";',
    "  }",
    "}",
    "",
  ].join("\n");
  await mkdir(join(root, "requests"), { recursive: true });
  await mkdir(join(root, "build", "lib", "requests"), { recursive: true });
  await writeFile(join(root, "requests", "models.ts"), source);
  await writeFile(join(root, "build", "lib", "requests", "models.ts"), source);

  const result = await service.context({
    root,
    query: "Content-Length",
    rg: true,
    limit: 10,
    rgOptions: {
      fixedStrings: true,
      beforeContext: 1,
      afterContext: 1,
    },
  });

  assert.equal(result.items.length, 1);
  assert.equal(result.items[0].file.relativePath, "requests/models.ts");
  assert.equal(result.items[0].occurrences.length, 3);
  assert.equal(
    result.items[0].container.metadata.symbolName,
    "prepareContentLength",
  );
  assert.equal(result.diagnostics.rg.rawOccurrences, 6);
  assert.equal(result.diagnostics.rg.uniqueOccurrences, 3);
  assert.equal(result.diagnostics.rg.groupsFound, 1);
  assert.equal(result.diagnostics.rg.generatedMirrorsCanonicalized, 3);
  assert.equal(result.diagnostics.rg.exactDuplicatesRemoved, 3);

  const formatted = formatAgentContextResult(result, {});
  assert.match(formatted, /matches: 3 at L3, L4, L5/);
  assert.match(
    formatted,
    /compacted: 6 occurrences -> 1 result; 3 generated mirror occurrences mapped to source/,
  );
  assert.equal((formatted.match(/Content-Length/g) ?? []).length, 2);
  assert.match(formatted, /^4:\t/m);
  assert.doesNotMatch(formatted, /^5[:-]\t/m);
  assert.doesNotMatch(formatted, /build\/lib/);
});

test("rg preserves every submatch on one line while rendering the line once", async (t) => {
  const { root, service } = await createRgService(t, "zvec-rg-submatches-");
  await writeFile(
    join(root, "values.ts"),
    'export function values() { return "needle needle"; }\n',
  );

  const result = await service.context({
    root,
    query: "needle",
    rg: true,
    rgOptions: { fixedStrings: true },
  });

  assert.equal(result.diagnostics.rg.rawOccurrences, 2);
  assert.equal(result.items.length, 1);
  assert.equal(result.items[0].occurrences.length, 2);
  const formatted = formatAgentContextResult(result, {});
  assert.match(formatted, /matches: 2 at L1, L1/);
  assert.equal((formatted.match(/needle needle/g) ?? []).length, 1);
});

test("rg retries a truncated occurrence scan until limit can select distinct symbols", async (t) => {
  const { root, service } = await createRgService(t, "zvec-rg-group-limit-");
  const noisyMatches = Array.from(
    { length: 205 },
    (_, index) => `  console.log("needle-${index}");`,
  );
  const source = [
    "export function noisy() {",
    ...noisyMatches,
    "}",
    "",
    "export function useful() {",
    '  return "needle-target";',
    "}",
    "",
  ].join("\n");
  await writeFile(join(root, "many.ts"), source);

  const result = await service.context({
    root,
    query: "needle",
    rg: true,
    limit: 2,
    rgOptions: { fixedStrings: true },
  });

  assert.equal(result.items.length, 2);
  assert.deepEqual(
    result.items.map((item) => item.container.metadata.symbolName),
    ["noisy", "useful"],
  );
  assert.equal(result.diagnostics.rg.scanLimit, 1_000);
  assert.equal(result.diagnostics.rg.rawOccurrences, 206);
  assert.equal(result.diagnostics.rg.groupsFound, 2);
  assert.equal(result.diagnostics.rg.truncated, false);
  assert.equal(result.coverage, "rg_exhaustive");
});

test("rg reports truncated coverage when logical groups exceed limit", async (t) => {
  const { root, service } = await createRgService(t, "zvec-rg-group-coverage-");
  await writeFile(
    join(root, "groups.ts"),
    [
      'export function first() { return "needle"; }',
      'export function second() { return "needle"; }',
      'export function third() { return "needle"; }',
      "",
    ].join("\n"),
  );

  const result = await service.context({
    root,
    query: "needle",
    rg: true,
    limit: 2,
    rgOptions: { fixedStrings: true },
  });

  assert.equal(result.diagnostics.rg.truncated, false);
  assert.equal(result.diagnostics.rg.groupTruncated, true);
  assert.equal(result.diagnostics.rg.groupsFound, 3);
  assert.equal(result.items.length, 2);
  assert.equal(result.coverage, "rg_truncated");
});

test("rg retries when demoted generated groups hide later source matches", async (t) => {
  const { root, service } = await createRgService(
    t,
    "zvec-rg-generated-retry-",
  );
  await mkdir(join(root, "dist"), { recursive: true });
  await mkdir(join(root, "src"), { recursive: true });
  await writeFile(
    join(root, "dist", "noisy.ts"),
    Array.from(
      { length: 205 },
      (_, index) =>
        `export function generated${index}() { return "needle-${index}"; }`,
    ).join("\n"),
  );
  await writeFile(
    join(root, "src", "useful.ts"),
    'export function useful() { return "needle-source"; }\n',
  );

  const result = await service.context({
    root,
    query: "needle",
    rg: true,
    limit: 1,
    rgOptions: {
      fixedStrings: true,
      extraArgs: ["--sort", "path"],
    },
  });

  assert.equal(result.items.length, 1);
  assert.equal(result.items[0].file.relativePath, "src/useful.ts");
  assert.equal(result.diagnostics.rg.scanLimit, 1_000);
  assert.equal(result.diagnostics.rg.rawOccurrences, 206);
  assert.equal(result.diagnostics.rg.generatedMatchesDemoted, 205);
});
