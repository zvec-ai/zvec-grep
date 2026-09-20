import assert from "node:assert/strict";
import test from "node:test";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { loadReleaseManifest, selectPlatform, stageZvecRuntime } from "./npm-package-core.mjs";

const manifest = loadReleaseManifest();

test("packages the SDK library and dictionaries beside the executable on every platform", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-zvec-runtime-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const source = join(root, "sdk");
  await mkdir(join(source, "data/jieba_dict"), { recursive: true });
  for (const file of ["jieba.dict.utf8", "hmm_model.utf8"]) {
    await writeFile(join(source, "data/jieba_dict", file), file);
  }
  for (const [platform, library] of Object.entries({
    darwin: "libzvec_c_api.dylib", linux: "libzvec_c_api.so", win32: "zvec_c_api.dll",
  })) {
    await writeFile(join(source, library), platform);
    const destination = join(root, platform, "bin");
    const files = await stageZvecRuntime(source, destination, platform);
    assert.equal(files.length, 3);
    for (const file of files) {
      assert.deepEqual(
        await readFile(join(root, platform, file)),
        await readFile(join(source, file.slice(4))),
      );
    }
  }
  await rm(join(source, "data/jieba_dict/hmm_model.utf8"));
  await assert.rejects(stageZvecRuntime(source, join(root, "incomplete/bin"), "linux"), /ENOENT/);
});

test("maps supported native npm targets", () => {
  assert.deepEqual(selectPlatform(manifest, "darwin", "arm64"), {
    package: "@zvec/zvec-grep-darwin-arm64",
    os: "darwin",
    cpu: "arm64",
    binary: "zg",
    target: "darwin-arm64",
  });
  assert.deepEqual(selectPlatform(manifest, "linux", "x64", { glibcVersionRuntime: "2.39" }), {
    package: "@zvec/zvec-grep-linux-x64-gnu",
    os: "linux",
    cpu: "x64",
    libc: "glibc",
    binary: "zg",
    target: "linux-x64-gnu",
  });
  assert.deepEqual(selectPlatform(manifest, "win32", "x64"), {
    package: "@zvec/zvec-grep-win32-x64-msvc",
    os: "win32",
    cpu: "x64",
    binary: "zg.exe",
    target: "win32-x64-msvc",
  });
});

test("rejects platforms without a local release target", () => {
  assert.throws(
    () => selectPlatform(manifest, "freebsd", "x64"),
    /unsupported npm platform/,
  );
  assert.throws(
    () => selectPlatform(manifest, "win32", "arm64"),
    /unsupported npm platform/,
  );
  assert.throws(
    () => selectPlatform(manifest, "linux", "riscv64", { glibcVersionRuntime: "2.39" }),
    /unsupported npm platform/,
  );
  assert.throws(
    () => selectPlatform(manifest, "linux", "x64"),
    /unsupported npm platform: linux-x64-musl/,
  );
});
