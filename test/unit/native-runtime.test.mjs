import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import {
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";
import { promisify } from "node:util";

const exec = promisify(execFile);
const platformDirectory = `zvec-native-${process.platform}-${process.arch}`;

async function fixture(t) {
  const root = await mkdtemp(join(tmpdir(), "zvec native runtime "));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, "dist/engine/storage"), { recursive: true });
  await writeFile(join(root, "package.json"), '{"type":"module"}');
  await copyFile(
    "dist/engine/storage/native.js",
    join(root, "dist/engine/storage/native.js"),
  );
  return root;
}

async function copyBinding(destination) {
  await mkdir(destination, { recursive: true });
  const packaged = join("native", platformDirectory, "zvec_node_binding.node");
  const source = await readFile(packaged).catch(() =>
    readFile("native/zvec_node_binding.node"),
  );
  await writeFile(join(destination, "zvec_node_binding.node"), source);
}

function load(root) {
  const url = pathToFileURL(join(root, "dist/engine/storage/native.js")).href;
  return exec(process.execPath, [
    "--input-type=module",
    "-e",
    `const b = await import(${JSON.stringify(url)}); if (b.ZVecIndexType.DISKANN === undefined) throw new Error('DiskANN missing');`,
  ]);
}

test("packaged runtime selects this platform, including paths with spaces", async (t) => {
  const root = await fixture(t);
  await copyBinding(join(root, "native", platformDirectory));
  await writeFile(
    join(root, "native/zvec_node_binding.node"),
    "invalid flat binding",
  );
  await load(root);
});

test("local source builds can still load the flat native directory", async (t) => {
  const root = await fixture(t);
  await copyBinding(join(root, "native"));
  await load(root);
});

test("a missing optimized binding never substitutes the release SDK", async (t) => {
  const root = await fixture(t);
  await mkdir(join(root, "node_modules/@zvec/zvec"), { recursive: true });
  await writeFile(
    join(root, "node_modules/@zvec/zvec/index.js"),
    "throw new Error('unexpected release SDK fallback');",
  );
  await assert.rejects(load(root), (error) => {
    assert.match(error.stderr, /MODULE_NOT_FOUND/);
    assert.match(error.stderr, /native[/\\]zvec_node_binding\.node/);
    assert.doesNotMatch(error.stderr, /unexpected release SDK fallback/);
    return true;
  });
});

test("packing rejects a missing runtime instead of emitting a broken package", async (t) => {
  const root = await fixture(t);
  await mkdir(join(root, "scripts"));
  for (const name of ["check-native.mjs", "build-zvec.py"]) {
    await copyFile(join("scripts", name), join(root, "scripts", name));
  }
  await assert.rejects(
    exec(process.execPath, [resolve(root, "scripts/check-native.mjs")]),
    /Native runtime missing/,
  );
});
