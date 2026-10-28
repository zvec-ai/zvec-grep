import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { indexIdentity, uniqueIndexes } from "../identity.mjs";
import { run } from "../../zg-retrieval/core/io.mjs";

async function fixture(t) {
  const source = await mkdtemp(join(tmpdir(), "browsecomp-storage-key-"));
  t.after(() => rm(source, { recursive: true, force: true }));
  await mkdir(join(source, "rust/crates/zg-engine/src/storage"), {
    recursive: true,
  });
  await mkdir(join(source, "rust/crates/zg-engine/src/workspace"), {
    recursive: true,
  });
  await writeFile(
    join(source, "rust/crates/zg-engine/src/workspace/mod.rs"),
    "pub(crate) const CURRENT_INDEX_VERSION: u32 = 2;\n",
  );
  await writeFile(
    join(source, "rust/crates/zg-engine/src/storage/record.rs"),
    "fn encode() {}\n",
  );
  await writeFile(
    join(source, "rust/crates/zg-engine/src/storage/tests.rs"),
    "test fixture\n",
  );
  await writeFile(
    join(source, "rust/Cargo.lock"),
    ["zvec-rust", "zvec-rust-sys", "rusqlite"]
      .map((name) => `[[package]]\nname = "${name}"\nversion = "1"\n`)
      .join("\n"),
  );
  await run("git", ["init", "--quiet", source]);
  const commit = async () => {
    await run("git", ["-C", source, "add", "."]);
    await run("git", [
      "-C",
      source,
      "-c",
      "core.hooksPath=/dev/null",
      "-c",
      "user.name=Benchmark Test",
      "-c",
      "user.email=bench@example.invalid",
      "commit",
      "--quiet",
      "-m",
      "fixture",
    ]);
  };
  await commit();
  const lock = {
    corpus: { revision: "pinned", count: 100195 },
    model: "local/potion-multilingual-128m",
    model_artifacts_sha256: "a".repeat(64),
    device: "cpu",
    index_max_filesize: 67108864,
  };
  return { source, commit, lock };
}

test("PR/query/test changes reuse the same index; storage changes invalidate it", async (t) => {
  const { source, commit, lock } = await fixture(t);
  const base = await indexIdentity(source, lock);
  await writeFile(join(source, "query.rs"), "new ranking implementation");
  await writeFile(
    join(source, "rust/crates/zg-engine/src/storage/tests.rs"),
    "different tests",
  );
  await commit();
  const query = await indexIdentity(source, lock);
  assert.notEqual(query.builder_commit, base.builder_commit);
  assert.equal(query.key, base.key);
  assert.equal(
    uniqueIndexes([
      { arm: "baseline", identity: base },
      { arm: "candidate", identity: query },
    ]).length,
    1,
  );
  await writeFile(
    join(source, "rust/crates/zg-engine/src/storage/record.rs"),
    "fn encode_new_storage() {}\n",
  );
  await commit();
  const storage = await indexIdentity(source, lock);
  assert.notEqual(storage.key, base.key);
  assert.equal(
    uniqueIndexes([
      { arm: "baseline", identity: base },
      { arm: "candidate", identity: storage },
    ]).length,
    2,
  );
});

test("storage version, backend dependency and pinned index inputs cannot alias", async (t) => {
  const { source, commit, lock } = await fixture(t);
  const base = await indexIdentity(source, lock);
  await writeFile(
    join(source, "rust/crates/zg-engine/src/workspace/mod.rs"),
    "pub(crate) const CURRENT_INDEX_VERSION: u32 = 3;\n",
  );
  await commit();
  const version = await indexIdentity(source, lock);
  assert.notEqual(version.key, base.key);
  const changed = await indexIdentity(source, {
    ...lock,
    model_artifacts_sha256: "b".repeat(64),
  });
  assert.notEqual(changed.key, version.key);
  await writeFile(
    join(source, "rust/Cargo.lock"),
    ["zvec-rust", "zvec-rust-sys", "rusqlite"]
      .map((name) => `[[package]]\nname = "${name}"\nversion = "2"\n`)
      .join("\n"),
  );
  await commit();
  assert.notEqual((await indexIdentity(source, lock)).key, version.key);
});

test("inline tests do not hide production storage items that follow them", async (t) => {
  const { source, commit, lock } = await fixture(t);
  const path = join(source, "rust/crates/zg-engine/src/storage/record.rs");
  const production = "fn encode() {}\n";
  const tests = (value) =>
    `\n#[cfg(test)]\nmod tests {\n    const TEST: u32 = ${value};\n}\n`;
  await writeFile(path, production + tests(1));
  await commit();
  const first = await indexIdentity(source, lock);
  await writeFile(path, production + tests(2));
  await commit();
  assert.equal((await indexIdentity(source, lock)).key, first.key);
  await writeFile(
    path,
    production + tests(2) + "\nfn new_storage_codec() {}\n",
  );
  await commit();
  assert.notEqual((await indexIdentity(source, lock)).key, first.key);
});

test("optional FP16 codec changes invalidate storage while legacy FP32 keys stay unchanged", async (t) => {
  const { source, commit, lock } = await fixture(t);
  const base = await indexIdentity(source, lock);
  const cargoPath = join(source, "rust/Cargo.lock");
  const cargo = await readFile(cargoPath, "utf8");
  const half = (version) =>
    `\n[[package]]\nname = "half"\nversion = "${version}"\n`;
  await writeFile(cargoPath, cargo + half("1"));
  await commit();
  assert.equal((await indexIdentity(source, lock)).key, base.key);
  const codec = join(source, "rust/crates/zg-zvec-fp16");
  await mkdir(join(codec, "src"), { recursive: true });
  const manifest = '[package]\nname = "zg-zvec-fp16"\n';
  await writeFile(join(codec, "Cargo.toml"), manifest);
  const production = "fn encode_fp16() {}\n";
  const tests = (value) =>
    `\n#[cfg(test)]\nmod tests {\n    const TEST: u32 = ${value};\n}\n`;
  await writeFile(join(codec, "src/lib.rs"), production + tests(1));
  await commit();
  const fp16 = await indexIdentity(source, lock);
  assert.equal(fp16.storage.index_version, 2);
  assert.notEqual(fp16.key, base.key);
  await writeFile(join(codec, "src/lib.rs"), production + tests(2));
  await commit();
  assert.equal((await indexIdentity(source, lock)).key, fp16.key);
  await writeFile(
    join(codec, "src/lib.rs"),
    "fn encode_fp16_differently() {}\n",
  );
  await commit();
  const changed = await indexIdentity(source, lock);
  assert.notEqual(changed.key, fp16.key);
  await writeFile(cargoPath, cargo + half("2"));
  await commit();
  const dependency = await indexIdentity(source, lock);
  assert.notEqual(dependency.key, changed.key);
  await writeFile(
    join(codec, "Cargo.toml"),
    manifest + "[features]\ncodec = []\n",
  );
  await commit();
  assert.notEqual((await indexIdentity(source, lock)).key, dependency.key);
});
