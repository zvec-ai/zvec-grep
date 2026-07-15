import assert from "node:assert/strict";
import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";

const workflowDirectory = new URL("../.github/workflows/", import.meta.url);
const files = (await readdir(workflowDirectory)).filter((file) =>
  /\.ya?ml$/.test(file),
);

assert.ok(files.length > 0, "no GitHub Actions workflows found");

for (const file of files) {
  const contents = await readFile(new URL(file, workflowDirectory), "utf8");
  for (const [lineIndex, line] of contents.split("\n").entries()) {
    const match = line.match(/^\s*(?:-\s*)?uses:\s*([^\s#]+)/);
    if (
      !match ||
      match[1].startsWith("./") ||
      match[1].startsWith("docker://")
    ) {
      continue;
    }
    assert.match(
      match[1],
      /^[^@]+@[0-9a-f]{40}$/,
      `${join(".github/workflows", file)}:${lineIndex + 1} must pin uses: to a full commit SHA`,
    );
  }
}
