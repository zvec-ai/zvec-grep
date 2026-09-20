import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { BaseEmbeddingModel } from "../dist/engine/models/embeddings.js";
import { createZvecGrep } from "../dist/index.js";

const execFileAsync = promisify(execFile);
const cliPath = resolve("dist/cli/index.js");

test("managed rg runs locally when indexed operations use server mode", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zvec-grep-rg-server-mode-"));
  await mkdir(join(root, "src"));
  await writeFile(
    join(root, "src", "answer.ts"),
    "export const exactNeedle = 42;\n",
  );
  t.after(async () => {
    await rm(root, { recursive: true, force: true });
  });

  const result = await execFileAsync(
    process.execPath,
    [cliPath, "--rg", "-n", "exactNeedle", "src"],
    {
      cwd: root,
      env: { ...process.env, ZVEC_GREP_MODE: "server" },
    },
  );

  assert.match(result.stdout, /src[\\/]answer\.ts\n {2}1:/);
  assert.match(result.stdout, /exactNeedle/);
});

test("managed rg bypasses workspace index state in direct and server modes", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zvec-grep-rg-fast-path-"));
  await mkdir(join(root, "src"), { recursive: true });
  await mkdir(join(root, ".zvec-grep"));
  await writeFile(
    join(root, "src", "answer.ts"),
    "export const fastPathNeedle = 42;\n",
  );
  await writeFile(join(root, ".zvec-grep", "manifest.json"), "{}\n");
  t.after(async () => {
    await rm(root, { recursive: true, force: true });
  });

  for (const mode of ["direct", "server"]) {
    const result = await execFileAsync(
      process.execPath,
      [cliPath, "--mode", mode, "--rg", "fastPathNeedle", "src"],
      { cwd: root, env: { ...process.env, NO_COLOR: "1" } },
    );

    assert.match(result.stdout, /src[\\/]answer\.ts/);
    assert.match(result.stdout, /fastPathNeedle/);
  }
});

test("managed rg ignores stale index status in direct and server modes", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zvec-grep-rg-stale-index-"));
  const source = join(root, "deleted.ts");
  await writeFile(source, "export const deletedNeedle = 42;\n");
  t.after(async () => {
    await rm(root, { recursive: true, force: true });
  });

  const service = await createZvecGrep({
    root,
    embeddingModel: new TestEmbeddingModel(),
  });
  try {
    await service.index();
  } finally {
    await service.close();
  }
  await rm(source);

  for (const mode of ["direct", "server"]) {
    const result = await execFileAsync(
      process.execPath,
      [cliPath, "--mode", mode, "--rg", "deletedNeedle"],
      { cwd: root, env: { ...process.env, NO_COLOR: "1" } },
    );

    assert.match(result.stdout, /^No matches\.$/m);
    assert.doesNotMatch(result.stderr, /status: possibly_stale/);
    assert.doesNotMatch(result.stderr, /results: served_from_current_index/);
  }
});

test("managed rg emits a compact file and adaptive symbol hierarchy", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zvec-grep-rg-grouped-"));
  await mkdir(join(root, "src"));
  await writeFile(
    join(root, "src", "grouped.py"),
    [
      "class Widget:",
      "    def answer(self):",
      "        grouped_needle = 42",
      "        return grouped_needle",
      "",
      "module_grouped_needle = True",
      "",
      "def declared_grouped_needle():",
      "    return True",
      "",
      "def singleton():",
      '    value = "grouped_needle"',
      "",
    ].join("\n"),
  );
  t.after(async () => {
    await rm(root, { recursive: true, force: true });
  });

  const result = await execFileAsync(
    process.execPath,
    [cliPath, "--rg", "-n", "grouped_needle", "src/grouped.py"],
    { cwd: root },
  );

  assert.match(
    result.stdout,
    /src\/grouped\.py\n {2}2-4 \[function Widget\.answer\]/,
  );
  assert.equal(result.stdout.match(/\[function Widget\.answer\]/g)?.length, 1);
  assert.match(result.stdout, / {4}3:\s+grouped_needle = 42/);
  assert.match(result.stdout, / {4}4:\s+return grouped_needle/);
  assert.match(result.stdout, / {2}6:\s+module_grouped_needle = True/);
  assert.match(result.stdout, / {2}8:\s+def declared_grouped_needle/);
  assert.doesNotMatch(result.stdout, /\[function declared_grouped_needle\]/);
  assert.match(
    result.stdout,
    / {2}\d+-\d+ \[function singleton\] \d+:\s+value = "grouped_needle"/,
  );
  assert.doesNotMatch(result.stdout, /^symbol:/mu);
});

class TestEmbeddingModel extends BaseEmbeddingModel {
  info = {
    reference: "test/deterministic",
    provider: "test",
    name: "deterministic",
    dimension: 8,
    metric: "cosine",
    inputKinds: ["text"],
    limits: { maxBatchSize: 64 },
  };

  async doEmbed(contents) {
    return {
      vectors: contents.map(() => [1, 0, 0, 0, 0, 0, 0, 0]),
      truncated: [],
    };
  }
}
