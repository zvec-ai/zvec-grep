import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import test from "node:test";
import { fileURLToPath } from "node:url";

const source = new URL("../../", import.meta.url);
const guideName = "09-portable-indexes.md";
const read = (name) => readFileSync(new URL(name, source), "utf8");

for (const entry of ["README.md", "docs/README.md", "docs/03-mcp.md"]) {
  test(`portability discovery: ${entry} links to the move guide`, () => {
    const content = read(entry);
    const links = [...content.matchAll(/\]\(([^)]+)\)/g)].map((m) => m[1]);
    const link = links.find((value) => value.endsWith(guideName));
    assert.ok(link, `${entry} must link directly to the move guide`);
    const target = new URL(link, new URL(entry, source));
    assert.match(readFileSync(target, "utf8"), /Move and reuse an index/);
  });
}

test("portability discovery: main CLI help lists all transfer actions", () => {
  const output = execFileSync(
    process.execPath,
    [fileURLToPath(new URL("dist/cli/index.js", source)), "--help"],
    { encoding: "utf8" },
  );
  for (const action of ["migrate-index", "export-index", "import-index"]) {
    assert.match(output, new RegExp(`^  --${action}\\s+\\S`, "m"));
  }
});

test("portability guide distinguishes relocation, migration and logical transfer", () => {
  const guide = read(`docs/${guideName}`);
  for (const heading of [
    "Relocation",
    "Migration",
    "Logical export and import",
  ]) {
    assert.ok(guide.includes(`## ${heading}`), `missing ${heading}`);
  }
  for (const action of ["migrate-index", "export-index", "import-index"]) {
    assert.ok(guide.includes(`zg --${action} `));
  }
});

test("portability guide states destination, credentials and model requirements", () => {
  const guide = read(`docs/${guideName}`);
  for (const requirement of [
    "must not already contain a workspace index",
    "same relative paths",
    "Credentials",
    "model files",
    "server",
    "separate operation",
  ]) {
    assert.ok(guide.includes(requirement), `missing ${requirement}`);
  }
});

test("portability guide limits vector reuse and describes verification", () => {
  const guide = read(`docs/${guideName}`);
  for (const requirement of [
    "unchanged documents",
    "Query embeddings",
    "changed documents",
    "index identity",
    "expected content",
    "deleted files",
    "split",
  ]) {
    assert.ok(guide.includes(requirement), `missing ${requirement}`);
  }
});

test("portability design states dated evidence and excludes untested MCP operations", () => {
  const design = read("docs/design/portable-workspace-index.md");
  assert.doesNotMatch(design, /behavior not\s+yet demonstrated/);
  for (const term of ["2026-10-04", "72422a9", "macOS", "Linux", "MCP"]) {
    assert.ok(design.includes(term), `missing evidence scope: ${term}`);
  }
});
