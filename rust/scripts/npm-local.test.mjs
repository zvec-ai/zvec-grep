import assert from "node:assert/strict";
import test from "node:test";
import { chmod, lstat, mkdtemp, mkdir, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  defaultReleaseVariant,
  existingNativeBuild,
  loadReleaseManifest,
  resolveBuildVariant,
  selectPlatform,
  stageRuntimeBundle,
  stageZvecRuntime,
} from "./npm-package-core.mjs";

const manifest = loadReleaseManifest();

test("packages the SDK library and dictionaries under lib on every platform", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-zvec-runtime-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const source = join(root, "sdk");
  await mkdir(join(source, "data/jieba_dict"), { recursive: true });
  for (const file of ["jieba.dict.utf8", "hmm_model.utf8"]) {
    await writeFile(join(source, "data/jieba_dict", file), file);
  }
  for (const [platform, library] of Object.entries({
    darwin: "libzvec_c_api.dylib",
    linux: "libzvec_c_api.so",
    win32: "zvec_c_api.dll",
  })) {
    await writeFile(join(source, library), platform);
    const destination = join(root, platform, "lib");
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
  await assert.rejects(
    stageZvecRuntime(source, join(root, "incomplete/lib"), "linux"),
    /ENOENT/,
  );
});

test("includes model runtime libraries but excludes build files and other platform libraries", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-model-runtime-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const source = join(root, "sdk");
  await mkdir(join(source, "data", "jieba_dict"), { recursive: true });
  for (const name of ["jieba.dict.utf8", "hmm_model.utf8"]) {
    await writeFile(join(source, "data", "jieba_dict", name), name);
  }
  const runtimeFiles = {
    darwin: ["libzvec_c_api.dylib", "libonnxruntime.1.dylib"],
    linux: ["libzvec_c_api.so", "libonnxruntime.so.1.24.2"],
    win32: ["zvec_c_api.dll", "onnxruntime.dll", "DirectML.dll"],
  };
  for (const name of [
    ...Object.values(runtimeFiles).flat(),
    "zg",
    "zg.exe",
    "zvec_c_api.lib",
  ]) {
    await writeFile(join(source, name), name);
  }
  for (const [platform, libraries] of Object.entries(runtimeFiles)) {
    const destination = join(root, platform, "lib");
    const files = await stageZvecRuntime(source, destination, platform);
    assert.deepEqual(
      files.filter((file) => !file.includes("/data/")).sort(),
      libraries.map((name) => `lib/${name}`).sort(),
    );
    for (const name of libraries) {
      assert.equal(await readFile(join(destination, name), "utf8"), name);
    }
  }
});

test("materializes cached model runtime symlinks and rejects broken runtime links", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-model-runtime-links-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  for (const [platform, zvec, runtimeLinks] of [
    ["darwin", "libzvec_c_api.dylib", {
      "libonnxruntime.dylib": "libonnxruntime.1.28.0.dylib",
      "libonnxruntime.1.28.0.dylib": "libonnxruntime.1.28.0.dylib",
    }],
    ["linux", "libzvec_c_api.so", {
      "libonnxruntime.so": "libonnxruntime.so.1.28.0",
      "libonnxruntime.so.1": "libonnxruntime.so.1.28.0",
      "libonnxruntime.so.1.28.0": "libonnxruntime.so.1.28.0",
    }],
    ["win32", "zvec_c_api.dll", {
      "onnxruntime.dll": "onnxruntime.dll",
      "DirectML.dll": "DirectML.dll",
    }],
  ]) {
    const source = join(root, platform, "profile");
    const cache = join(root, platform, "ort-cache");
    const destination = join(root, platform, "package", "lib");
    await mkdir(join(source, "data", "jieba_dict"), { recursive: true });
    await mkdir(cache, { recursive: true });
    for (const name of ["jieba.dict.utf8", "hmm_model.utf8"]) {
      await writeFile(join(source, "data", "jieba_dict", name), name);
    }
    await writeFile(join(source, zvec), zvec);
    for (const name of new Set(Object.values(runtimeLinks))) {
      await writeFile(join(cache, name), `${platform}:${name}`);
    }
    for (const [name, cachedName] of Object.entries(runtimeLinks)) {
      try {
        await symlink(join(cache, cachedName), join(source, name), "file");
      } catch (error) {
        if (process.platform === "win32" && ["EPERM", "EACCES"].includes(error.code)) {
          t.skip("creating symbolic links requires Windows Developer Mode or elevated privileges; regular DLL staging is covered separately");
          return;
        }
        throw error;
      }
      assert.equal((await lstat(join(source, name))).isSymbolicLink(), true);
    }

    const files = await stageZvecRuntime(source, destination, platform);
    assert.deepEqual(files.sort(), [
      `lib/${zvec}`,
      "lib/data/jieba_dict/jieba.dict.utf8",
      "lib/data/jieba_dict/hmm_model.utf8",
      ...Object.keys(runtimeLinks).map((name) => `lib/${name}`),
    ].sort());
    await rm(cache, { recursive: true });
    for (const [name, cachedName] of Object.entries(runtimeLinks)) {
      const path = join(destination, name);
      assert.equal((await lstat(path)).isFile(), true);
      assert.equal(await readFile(path, "utf8"), `${platform}:${cachedName}`);
    }
    await assert.rejects(
      stageZvecRuntime(source, join(root, platform, "broken", "lib"), platform),
      { code: "ENOENT" },
    );
  }
});

test("stages a complete native bundle with one binary and sibling libraries", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-native-bundle-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const zvecLibraries = {
    darwin: "libzvec_c_api.dylib",
    linux: "libzvec_c_api.so",
    win32: "zvec_c_api.dll",
  };
  for (const entry of manifest.platforms) {
    const configuration = {
      binary: entry.binary,
      zvec: zvecLibraries[entry.os],
      backend: `runtime-${entry.target}.native`,
    };
    const source = join(root, `${entry.target}-source`);
    const destination = join(root, `${entry.target}-package`);
    await mkdir(join(source, "bin"), { recursive: true });
    await mkdir(join(source, "lib/data/jieba_dict"), { recursive: true });
    await writeFile(join(source, "bin", configuration.binary), `${entry.target} binary\n`);
    await chmod(join(source, "bin", configuration.binary), 0o755);
    await writeFile(join(source, "lib", configuration.zvec), `${entry.target} zvec\n`);
    await writeFile(join(source, "lib", configuration.backend), `${entry.target} backend\n`);
    await writeFile(join(source, "lib/data/jieba_dict/jieba.dict.utf8"), "dictionary\n");
    await writeFile(join(source, "lib/data/jieba_dict/hmm_model.utf8"), "model\n");

    const files = await stageRuntimeBundle(
      source,
      destination,
      entry.os,
      configuration.binary,
    );
    assert.deepEqual(files, [
      `bin/${configuration.binary}`,
      `lib/${configuration.backend}`,
      "lib/data/jieba_dict/hmm_model.utf8",
      "lib/data/jieba_dict/jieba.dict.utf8",
      `lib/${configuration.zvec}`,
    ].sort());
    for (const file of files) {
      assert.deepEqual(
        await readFile(join(root, `${entry.target}-package`, file)),
        await readFile(join(source, file)),
      );
    }
    await writeFile(join(source, "bin", "unwanted-library"), "must be in lib");
    await assert.rejects(stageRuntimeBundle(source, destination, entry.os, configuration.binary),
      /bin\/ must contain only/);
    await rm(join(source, "bin", "unwanted-library"));
    await writeFile(join(source, "build.log"), "not a runtime artifact");
    await assert.rejects(stageRuntimeBundle(source, destination, entry.os, configuration.binary),
      /only bin\/ and lib\//);
  }
});

test("keeps only CUDA plugins in the secondary backend directory", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-dual-cuda-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  for (const [platform, binary, zvec, cuda, core] of [
    ["linux", "zg", "libzvec_c_api.so", "libggml-cuda.so", "libggml-base.so.0"],
    ["win32", "zg.exe", "zvec_c_api.dll", "ggml-cuda.dll", "ggml-base.dll"],
  ]) {
    const source = join(root, platform, "source");
    await mkdir(join(source, "bin"), { recursive: true });
    await mkdir(join(source, "lib/cuda12"), { recursive: true });
    await mkdir(join(source, "lib/data/jieba_dict"), { recursive: true });
    await writeFile(join(source, "bin", binary), binary);
    for (const file of [zvec, cuda, core, `cuda12/${cuda}`,
      "data/jieba_dict/jieba.dict.utf8", "data/jieba_dict/hmm_model.utf8"]) {
      await writeFile(join(source, "lib", file), file);
    }
    const files = await stageRuntimeBundle(source, join(root, platform), platform, binary);
    assert.deepEqual(files.filter((file) => file.startsWith("lib/cuda12/")), [`lib/cuda12/${cuda}`]);
    assert.equal(files.filter((file) => file.endsWith(core)).length, 1);
  }
});

test("rejects symbolic links in a native bundle", async (t) => {
  if (process.platform === "win32") {
    t.skip("creating symbolic links requires elevated Windows privileges");
    return;
  }
  const root = await mkdtemp(join(tmpdir(), "zg-native-bundle-link-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const source = join(root, "source");
  await mkdir(join(source, "bin"), { recursive: true });
  await mkdir(join(source, "lib/data/jieba_dict"), { recursive: true });
  await writeFile(join(source, "bin/zg"), "binary\n");
  await writeFile(join(source, "lib/libzvec_c_api.so"), "zvec\n");
  await writeFile(join(source, "lib/libggml.so.0"), "ggml\n");
  await writeFile(join(source, "lib/data/jieba_dict/jieba.dict.utf8"), "dictionary\n");
  await writeFile(join(source, "lib/data/jieba_dict/hmm_model.utf8"), "model\n");
  await symlink("libggml.so.0", join(source, "lib/libggml.so"));
  await assert.rejects(
    stageRuntimeBundle(source, join(root, "package"), "linux", "zg"),
    /not a regular file/,
  );
});

test("maps supported native npm targets", () => {
  assert.deepEqual(selectPlatform(manifest, "darwin", "arm64"), {
    package: "@zvec/zvec-grep-darwin-arm64",
    os: "darwin",
    cpu: "arm64",
    binary: "zg",
    target: "darwin-arm64",
  });
  assert.deepEqual(selectPlatform(manifest, "darwin", "x64"), {
    package: "@zvec/zvec-grep-darwin-x64",
    os: "darwin",
    cpu: "x64",
    binary: "zg",
    target: "darwin-x64",
  });
  assert.deepEqual(
    selectPlatform(manifest, "linux", "x64", { glibcVersionRuntime: "2.39" }),
    {
      package: "@zvec/zvec-grep-linux-x64-gnu",
      os: "linux",
      cpu: "x64",
      libc: "glibc",
      binary: "zg",
      defaultVariant: "cuda",
      target: "linux-x64-gnu",
    },
  );
  assert.deepEqual(selectPlatform(manifest, "win32", "x64"), {
    package: "@zvec/zvec-grep-win32-x64-msvc",
    os: "win32",
    cpu: "x64",
    binary: "zg.exe",
    defaultVariant: "cuda",
    target: "win32-x64-msvc",
  });
});

test("defaults Linux and Windows x64 releases to CUDA-capable bundles", () => {
  const cudaTargets = new Set(["linux-x64-gnu", "win32-x64-msvc"]);
  for (const entry of manifest.platforms) {
    assert.equal(
      defaultReleaseVariant(entry),
      cudaTargets.has(entry.target) ? "cuda" : "cpu",
    );
  }
});

test("prefers CUDA, permits absent toolkits, and never hides broken CUDA builds", () => {
  for (const entry of manifest.platforms.filter((entry) => entry.defaultVariant === "cuda")) {
    assert.equal(resolveBuildVariant(entry, false, () => "Cuda compilation tools, release 13.1, V13.1.1"), "cuda");
    assert.equal(resolveBuildVariant(entry, false, () => { throw Object.assign(new Error("not found"), { code: "ENOENT" }); }), "cpu");
    assert.equal(resolveBuildVariant(entry, true, () => { throw new Error("must not probe"); }), "cpu");
    assert.throws(() => resolveBuildVariant(entry, false, () => "release 12.4, V12.4.1"), /requires nvcc 13.1/);
    assert.throws(() => resolveBuildVariant(entry, false, () => { throw new Error("compiler failed"); }), /compiler failed/);
  }
  assert.equal(resolveBuildVariant(selectPlatform(manifest, "darwin", "arm64"), false,
    () => { throw new Error("must not probe"); }), "cpu");
});

test("no-build uses the last successful variant rather than stale CUDA artifacts", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-build-record-"));
  const previous = process.env.CARGO_TARGET_DIR;
  process.env.CARGO_TARGET_DIR = root;
  t.after(async () => {
    if (previous === undefined) delete process.env.CARGO_TARGET_DIR;
    else process.env.CARGO_TARGET_DIR = previous;
    await rm(root, { recursive: true, force: true });
  });
  const entry = selectPlatform(manifest, "win32", "x64");
  const cuda = { target: entry.target, variant: "cuda", profileDir: join(root, "cuda/release"), bundleDir: join(root, "cuda/release/zg-cuda") };
  const cpu = { target: entry.target, variant: "cpu", profileDir: join(root, "cpu/release") };
  await mkdir(cuda.bundleDir, { recursive: true });
  assert.equal((await existingNativeBuild(entry)).bundleDir, undefined);
  await writeFile(join(root, "native-build.json"), JSON.stringify(cuda));
  assert.deepEqual(await existingNativeBuild(entry), cuda);
  assert.equal((await existingNativeBuild(entry, { cpuOnly: true })).bundleDir, undefined);
  await writeFile(join(root, "native-build.json"), JSON.stringify(cpu));
  assert.deepEqual(await existingNativeBuild(entry), cpu);
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
    () =>
      selectPlatform(manifest, "linux", "riscv64", {
        glibcVersionRuntime: "2.39",
      }),
    /unsupported npm platform/,
  );
  assert.throws(
    () => selectPlatform(manifest, "linux", "x64"),
    /unsupported npm platform: linux-x64-musl/,
  );
});
