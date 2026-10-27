import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { chmod, lstat, mkdtemp, mkdir, readFile, rm, symlink, truncate, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  buildNative,
  defaultReleaseVariant,
  MAX_NPM_TARBALL_BYTES,
  existingNativeBuild,
  loadReleaseManifest,
  resolveBuildVariant,
  selectPlatform,
  stageRuntimeBundle,
  stageComponentPayload,
  stageZvecRuntime,
  verifyTarballSize,
} from "./npm-package-core.mjs";

const manifest = loadReleaseManifest();

test("packages the SDK library with its embedded dictionary under lib on every platform", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-zvec-runtime-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const source = join(root, "sdk");
  await mkdir(source);
  for (const [platform, library] of Object.entries({
    darwin: "libzvec_c_api.dylib",
    linux: "libzvec_c_api.so",
    win32: "zvec_c_api.dll",
  })) {
    await writeFile(join(source, library), platform);
    const destination = join(root, platform, "lib");
    const files = await stageZvecRuntime(source, destination, platform);
    assert.deepEqual(files, [`lib/${library}`]);
    for (const file of files) {
      assert.deepEqual(
        await readFile(join(root, platform, file)),
        await readFile(join(source, file.slice(4))),
      );
    }
  }
  await rm(join(source, "libzvec_c_api.so"));
  await assert.rejects(
    stageZvecRuntime(source, join(root, "incomplete/lib"), "linux"),
    /ENOENT/,
  );
});

test("includes model runtime libraries but excludes build files and other platform libraries", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-model-runtime-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const source = join(root, "sdk");
  await mkdir(source);
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
      files.sort(),
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
    await mkdir(source, { recursive: true });
    await mkdir(cache, { recursive: true });
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
    await mkdir(join(source, "lib"), { recursive: true });
    await writeFile(join(source, "bin", configuration.binary), `${entry.target} binary\n`);
    await chmod(join(source, "bin", configuration.binary), 0o755);
    await writeFile(join(source, "lib", configuration.zvec), `${entry.target} zvec\n`);
    await writeFile(join(source, "lib", configuration.backend), `${entry.target} backend\n`);

    const files = await stageRuntimeBundle(
      source,
      destination,
      entry.os,
      configuration.binary,
    );
    assert.deepEqual(files, [
      `bin/${configuration.binary}`,
      `lib/${configuration.backend}`,
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
    await writeFile(join(source, "bin", binary), binary);
    for (const file of [zvec, cuda, core, `cuda12/${cuda}`]) {
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
  await mkdir(join(source, "lib"), { recursive: true });
  await writeFile(join(source, "bin/zg"), "binary\n");
  await writeFile(join(source, "lib/libzvec_c_api.so"), "zvec\n");
  await writeFile(join(source, "lib/libggml.so.0"), "ggml\n");
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
      components: manifest.platforms.find((entry) => entry.target === "linux-x64-gnu").components,
    },
  );
  assert.deepEqual(selectPlatform(manifest, "win32", "x64"), {
    package: "@zvec/zvec-grep-win32-x64-msvc",
    os: "win32",
    cpu: "x64",
    binary: "zg.exe",
    defaultVariant: "cuda",
    target: "win32-x64-msvc",
    components: manifest.platforms.find((entry) => entry.target === "win32-x64-msvc").components,
  });
  assert.deepEqual(selectPlatform(manifest, "win32", "arm64"), {
    package: "@zvec/zvec-grep-win32-arm64-msvc",
    os: "win32",
    cpu: "arm64",
    binary: "zg.exe",
    target: "win32-arm64-msvc",
  });
});

test("splits the Linux ORT provider into ordered bounded-memory component payloads", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-native-components-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const entry = manifest.platforms.find((entry) => entry.target === "linux-x64-gnu");
  const components = entry.components.filter((component) => component.payload.startsWith("parts/"));
  const source = Buffer.alloc(2 * 1024 * 1024 + 37);
  for (let i = 0; i < source.length; i++) source[i] = i % 251;
  await mkdir(join(root, "source/lib"), { recursive: true });
  await writeFile(join(root, "source", components[0].file), source);
  const parts = [];
  for (const [index, component] of components.entries()) {
    const destination = join(root, `part-${index}`);
    const checksum = await stageComponentPayload(join(root, "source"), destination, entry, component);
    const bytes = await readFile(join(destination, component.payload));
    assert.equal(checksum.bytes, bytes.length);
    assert.equal(checksum.sha256.length, 64);
    parts.push(bytes);
  }
  assert.deepEqual(Buffer.concat(parts), source);
});

test("enforces the compressed npm tarball budget at the exact boundary", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-npm-size-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const path = join(root, "candidate.tgz");
  await writeFile(path, "");
  await truncate(path, MAX_NPM_TARBALL_BYTES);
  assert.equal(await verifyTarballSize(path), MAX_NPM_TARBALL_BYTES);
  await truncate(path, MAX_NPM_TARBALL_BYTES + 1);
  await assert.rejects(verifyTarballSize(path), /exceeds 350 MiB publication budget/);
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

test("explicit prebuilt mode selects CUDA without probing nvcc and still honors CPU-only", () => {
  const noCompiler = () => { throw new Error("prebuilt must not probe nvcc"); };
  for (const entry of manifest.platforms) {
    assert.equal(resolveBuildVariant(entry, false, noCompiler, "prebuilt"), defaultReleaseVariant(entry));
    assert.equal(resolveBuildVariant(entry, true, noCompiler, "prebuilt"), "cpu");
  }
});

test("Windows ARM64 publishing requires its CPU SDK without source compiler overrides", async () => {
  const entry = selectPlatform(manifest, "win32", "arm64");
  assert.equal(resolveBuildVariant(entry, false,
    () => { throw new Error("Windows ARM64 must not probe nvcc"); }, "prebuilt"), "cpu");
  const workflow = await readFile(new URL("../../.github/workflows/rust-package-build.yml", import.meta.url), "utf8");
  assert.match(workflow, /^  LLAMA_CPP_BUILD_MODE: prebuilt$/m);
  assert.doesNotMatch(workflow,
    /["'](?:CC|CXX|CXXFLAGS|CMAKE_GENERATOR|CMAKE_C_COMPILER_TARGET|CMAKE_CXX_COMPILER_TARGET)=/);
});

test("CUDA fallback uses the primary Cargo source record, including cached builds and overrides", async (t) => {
  // Exercise real Cargo build-script metadata without compiling CUDA or
  // downloading llama.cpp. Kernel compilation/inference stays in native CI.
  const root = await mkdtemp(join(tmpdir(), "zg-fallback-source-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const project = join(root, "bindings");
  const target = join(root, "target");
  const cached = join(root, "source cache 中文");
  const override = join(root, "source override 中文");
  const revision = "5f55650a78f92aff4d48d671423e888fac0469ff";
  for (const directory of [cached, override, join(project, "llama.cpp")]) {
    await mkdir(directory, { recursive: true });
    await writeFile(join(directory, "CMakeLists.txt"), "# source path fixture\n");
  }
  await mkdir(join(project, "src"));
  await writeFile(join(project, "Cargo.toml"),
    '[package]\nname = "llama-cpp-sys-2"\nversion = "0.0.0"\nedition = "2021"\nlinks = "llama"\n');
  await writeFile(join(project, "Cargo.lock"),
    'version = 4\n[[package]]\nname = "llama-cpp-sys-2"\nversion = "0.0.0"\n');
  await writeFile(join(project, "src/lib.rs"), "");
  await writeFile(join(project, "build.rs"), [
    "fn main() {",
    'println!("cargo:rerun-if-env-changed=ZG_TEST_SOURCE_DIR");',
    'println!("cargo:source_dir={}", std::env::var("ZG_TEST_SOURCE_DIR").unwrap());',
    `println!("cargo:source_revision=${revision}");`,
    "}",
  ].join("\n"));
  const messages = join(root, "cargo-build.json");
  const python = process.platform === "win32" ? "python" : "python3";
  const scripts = fileURLToPath(new URL("./cuda", import.meta.url));
  const inspect = (code, ...args) => {
    const result = spawnSync(python, ["-c", String.raw`
import importlib.util, json, pathlib, sys
sys.path.insert(0, sys.argv[1])
spec = importlib.util.spec_from_file_location("fallback", pathlib.Path(sys.argv[1]) / "build-fallback.py")
fallback = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fallback)
messages = pathlib.Path(sys.argv[2])
` + code, scripts, messages, ...args], { encoding: "utf8", timeout: 30_000 });
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stdout + result.stderr);
  };
  for (const [index, source] of [cached, cached, override].entries()) {
    const result = spawnSync("cargo", ["build", "--locked", "--offline", "--message-format=json"], {
      cwd: project, env: { ...process.env, CARGO_TARGET_DIR: target, ZG_TEST_SOURCE_DIR: source },
      encoding: "utf8", timeout: 120_000,
    });
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stdout + result.stderr);
    await writeFile(messages, result.stdout);
    const records = result.stdout.trim().split(/\r?\n/).map((line) => JSON.parse(line));
    if (index === 1) assert.ok(records.some((r) => r.reason === "compiler-artifact" && r.fresh));
    inspect(String.raw`
source, revision = fallback.source_build(messages)
assert source == pathlib.Path(sys.argv[3]), (source, sys.argv[3])
assert revision == sys.argv[4]
command = fallback.configure(source, source / "build", source / "cuda", fallback.DEFAULT_ARCHITECTURES)
assert command[command.index("-S") + 1] == str(source)
`, source, revision);
  }
  inspect(String.raw`
records = fallback.cargo_records(messages)
build = next(r for r in records if r.get("reason") == "build-script-executed")
output = pathlib.Path(build["out_dir"]).parent / "output"
original = output.read_text(encoding="utf-8")
def reject(expected):
    try:
        fallback.source_build(messages)
    except RuntimeError as error:
        assert expected in str(error), str(error)
    else:
        raise AssertionError("invalid primary source accepted")
for text, error in [
    ("cargo:source_revision=" + "a" * 40, "source_dir"),
    (original + "\ncargo:source_dir=/stale\n", "source_dir"),
    (original.replace(sys.argv[3], "relative/source"), "source is unavailable"),
    (original.replace(sys.argv[3], str(output.parent / "missing")), "source is unavailable"),
    (original.replace(sys.argv[4], "not-a-revision"), "source_revision"),
]:
    output.write_text(text, encoding="utf-8")
    reject(error)
output.write_text(original, encoding="utf-8")
for changed, error in [
    (records[:-1], "finish successfully"),
    ([*records[:-1], {"reason": "build-finished", "success": False}], "finish successfully"),
    ([r for r in records if r is not build], "one active"),
    ([build, *records], "one active"),
]:
    messages.write_text("\n".join(json.dumps(r) for r in changed), encoding="utf-8")
    reject(error)
`, override, revision);
});

test("GPU test bundle packaging selects current files and verifies relocated payloads", () => {
  // File-transport coverage only; real CPU/CUDA inference remains in the workflow.
  const result = spawnSync(process.platform === "win32" ? "python" : "python3", ["-c", String.raw`
import json, os, pathlib, sys, tarfile, tempfile, types
sys.path.insert(0, sys.argv[1])
import runtime
with tempfile.TemporaryDirectory(prefix="zg-test-bundle-") as temporary:
    root = pathlib.Path(temporary)
    backend = root / "SDK 中文/lib"
    profile = root / "build/debug"
    for directory in (backend / "cuda12", profile / "deps", root / "zvec", root / "stale"):
        directory.mkdir(parents=True)
    for name in ("llama.dll", "ggml.dll", "ggml-base.dll", "ggml-cpu.dll", "ggml-cuda.dll",
                 "cuda12/ggml-cuda.dll", "llama.lib"):
        (backend / name).write_bytes(name.encode())
    (backend.parent / "manifest.json").write_text(json.dumps({
        "files": {p.relative_to(backend.parent).as_posix(): runtime.digest(p)
                  for p in backend.rglob("*") if p.is_file()}}))
    for name in ("onnxruntime.dll", "onnxruntime_providers_shared.dll", "onnxruntime_providers_cuda.dll"):
        (profile / name).write_bytes(name.encode())
    (root / "zvec/zvec_c_api.dll").write_bytes(b"current zvec")
    (root / "stale/zvec_c_api.dll").write_bytes(b"must not be selected")
    records = [
        {"reason": "build-script-executed", "package_id": "llama-cpp-2",
         "env": [["GGML_BACKENDS_DIR", str(backend)]]},
        {"reason": "build-script-executed", "package_id": "zvec-rust-sys",
         "linked_paths": ["native=" + str(root / "zvec")]},
    ]
    for name in ("zg_engine", "zg_llama_native"):
        binary = profile / "deps" / (name + ".exe")
        binary.write_bytes(name.encode())
        binary.chmod(0o755)
        records.append({"reason": "compiler-artifact", "target": {"name": name},
                        "profile": {"test": True}, "features": ["cuda"], "executable": str(binary)})
    messages = root / "messages.json"
    def save():
        messages.write_text("\n".join(json.dumps(r) for r in records), encoding="utf-8")
    save()
    os.environ["SOURCE_SHA"] = "a" * 40
    runtime.stage_test_bundle(messages, root / "tests.tgz")
    relocated = root / "different path"
    with tarfile.open(root / "tests.tgz") as archive:
        # This archive was generated above, not downloaded. CI uses Python 3.12's
        # data filter; the local packaging test also supports older macOS Python.
        assert all(not m.issym() and not m.islnk() and
                   pathlib.PurePosixPath(m.name).parts[0] == "cuda-tests" and
                   ".." not in pathlib.PurePosixPath(m.name).parts for m in archive.getmembers())
        archive.extractall(relocated)
    bundle = relocated / "cuda-tests"
    manifest = json.loads((bundle / "manifest.json").read_text())
    assert not any("cuda12" in p or p.endswith(".lib") for p in manifest["files"])
    assert (bundle / "lib/zvec_c_api.dll").read_bytes() == b"current zvec"
    if os.name != "nt":
        assert (bundle / "bin/zg_engine.exe").stat().st_mode & 0o111
    assert str(root) not in json.dumps(manifest)
    runtime.os = types.SimpleNamespace(name="nt", environ=os.environ)
    runtime.verify_test_bundle(bundle)
    def rejected(action, message):
        try:
            action()
        except (RuntimeError, FileNotFoundError) as error:
            assert message in str(error), str(error)
        else:
            raise AssertionError("invalid bundle was accepted")
    os.environ["SOURCE_SHA"] = "b" * 40
    rejected(lambda: runtime.verify_test_bundle(bundle), "source commit")
    os.environ["SOURCE_SHA"] = "a" * 40
    (bundle / "bin/zg_engine.exe").write_bytes(b"changed")
    rejected(lambda: runtime.verify_test_bundle(bundle), "SHA-256")
    runtime.os = os
    records[-2]["features"] = []
    save()
    rejected(lambda: runtime.stage_test_bundle(messages, root / "invalid.tgz"), "CUDA-enabled")
    records[-2]["features"] = ["cuda"]
    save()
    (profile / "onnxruntime_providers_cuda.dll").unlink()
    rejected(lambda: runtime.stage_test_bundle(messages, root / "missing.tgz"), "onnxruntime_providers_cuda")
`, fileURLToPath(new URL("./cuda", import.meta.url))], { encoding: "utf8", timeout: 30_000 });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stdout + result.stderr);
});

test("Windows CUDA Cargo capture works with Legacy arguments, UTF-8 paths and build failures", async (t) => {
  // Use Windows PowerShell 5 itself on Windows; pwsh can exercise Legacy
  // native argument passing locally without a Windows or CUDA toolchain.
  const shell = process.platform === "win32" ? "powershell.exe" : "pwsh";
  const probe = spawnSync(shell, ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "exit 0"]);
  if (probe.error?.code === "ENOENT" && process.platform !== "win32") {
    t.skip("PowerShell is unavailable; Windows runs this with powershell.exe");
    return;
  }
  assert.ifError(probe.error);
  assert.equal(probe.status, 0);
  const root = await mkdtemp(join(tmpdir(), "zg-cargo-capture-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const project = join(root, "project 中文");
  const target = join(root, "target 中文");
  await mkdir(join(project, "src"), { recursive: true });
  await mkdir(target);
  await writeFile(join(project, "Cargo.toml"),
    '[package]\nname = "zg"\nversion = "0.0.0"\nedition = "2021"\n[features]\ncuda = []\n');
  await writeFile(join(project, "Cargo.lock"),
    'version = 4\n[[package]]\nname = "zg"\nversion = "0.0.0"\n');
  await writeFile(join(project, "src/main.rs"), "fn main() {}\n");
  const source = await readFile(new URL("./cuda/build.ps1", import.meta.url), "utf8");
  const start = source.indexOf('$messages = Join-Path');
  const end = source.indexOf('$profileDirectory = Join-Path', start);
  assert.ok(start >= 0 && end > start, "locate the real Cargo capture and exit-code guard");
  const script = join(root, "capture.ps1");
  await writeFile(script, [
    "param([string]$targetDirectory)",
    '$ErrorActionPreference = "Stop"',
    '$PSNativeCommandArgumentPassing = "Legacy"',
    ...(process.platform === "win32" ? [] : ["Set-Alias -Name python -Value python3"]),
    source.slice(start, end),
    "exit $LASTEXITCODE",
  ].join("\n"));
  const execute = () => spawnSync(shell,
    ["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", script, target],
    { cwd: project, env: { ...process.env, CARGO_TARGET_DIR: target, CARGO_TERM_COLOR: "never" },
      encoding: "utf8", timeout: 120_000 });
  const records = async () => (await readFile(join(target, "cuda-build.json"), "utf8"))
    .trim().split(/\r?\n/).map((line) => JSON.parse(line));
  const success = execute();
  assert.ifError(success.error);
  assert.equal(success.status, 0, success.stdout + success.stderr);
  const output = await records();
  assert.equal(output.at(-1).success, true);
  assert.ok(output.some((record) => record.reason === "compiler-artifact" &&
    record.executable && record.manifest_path.includes("project 中文") &&
    record.features.includes("cuda")));
  await writeFile(join(project, "src/main.rs"), "compile_error!(\"expected capture failure\");\nfn main() {}\n");
  const failure = execute();
  assert.ifError(failure.error);
  assert.notEqual(failure.status, 0);
  assert.match(failure.stdout + failure.stderr, /cargo build failed with exit code 101/);
  assert.equal((await records()).at(-1).success, false);
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

test("manual no-record CPU staging rejects CUDA leftovers before copying any runtime", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-mixed-cargo-profile-"));
  const previous = process.env.CARGO_TARGET_DIR;
  t.after(async () => {
    if (previous === undefined) delete process.env.CARGO_TARGET_DIR;
    else process.env.CARGO_TARGET_DIR = previous;
    await rm(root, { recursive: true, force: true });
  });
  for (const [target, cpuFiles, staleFiles] of [
    ["linux-x64-gnu", ["libzvec_c_api.so", "libonnxruntime.so.1.27.0"], [
      "libllama.so.0", "libggml.so.0", "libggml-base.so.0", "libggml-cpu.so",
      "libggml-cuda.so", "libonnxruntime_providers_cuda.so", "libonnxruntime_providers_shared.so",
      "libcudart.so.13", "libcublas.so.13", "libcudnn.so.9", "unknown-runtime.so",
    ]],
    ["win32-x64-msvc", ["zvec_c_api.dll", "onnxruntime.dll", "DirectML.dll"], [
      "llama.dll", "ggml.dll", "ggml-base.dll", "ggml-cpu.dll", "GGML-CUDA.DLL",
      "onnxruntime_providers_cuda.dll", "onnxruntime_providers_shared.dll",
      "cudart64_13.dll", "cublas64_13.dll", "cudnn64_9.dll", "unknown-runtime.dll",
    ]],
  ]) {
    const entry = manifest.platforms.find((entry) => entry.target === target);
    process.env.CARGO_TARGET_DIR = join(root, target);
    const profile = join(process.env.CARGO_TARGET_DIR, "release");
    await mkdir(profile, { recursive: true });
    for (const file of cpuFiles) await writeFile(join(profile, file), `current CPU ${file}`);
    await writeFile(join(profile, entry.binary), "old CUDA executable fixture");
    // Model Cargo replacing the executable without removing sibling libraries.
    await writeFile(join(profile, entry.binary), "new CPU executable fixture");
    const build = await existingNativeBuild(entry);
    assert.deepEqual(build, { variant: "cpu", profileDir: profile });
    await assert.rejects(lstat(join(process.env.CARGO_TARGET_DIR, "native-build.json")), { code: "ENOENT" });
    const destination = join(root, target, "package/lib");
    for (const file of staleFiles) {
      await writeFile(join(profile, file), `old CUDA ${file}`);
      await assert.rejects(stageZvecRuntime(build.profileDir, destination, entry.os), (error) => {
        assert.match(error.message, /refusing CPU-only packaging/);
        assert.ok(error.message.includes(file));
        assert.match(error.message, /fresh CARGO_TARGET_DIR/);
        assert.match(error.message, /--bundle-dir/);
        return true;
      });
      await assert.rejects(lstat(destination), { code: "ENOENT" });
      assert.equal(await readFile(join(profile, file), "utf8"), `old CUDA ${file}`);
      await rm(join(profile, file));
    }
    // Nested build/bundle outputs are not scanned or copied into CPU packages.
    await mkdir(join(profile, "cuda12"));
    await writeFile(join(profile, "cuda12", staleFiles[4]), "old fallback");
    const files = await stageZvecRuntime(build.profileDir, destination, entry.os);
    assert.deepEqual(files.sort(), cpuFiles.map((file) => `lib/${file}`).sort());
    for (const file of cpuFiles) {
      assert.equal(await readFile(join(destination, file), "utf8"), `current CPU ${file}`);
    }
  }
});

test("a failed CUDA rebuild invalidates reuse until a complete build succeeds", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-failed-build-"));
  const previous = process.env.CARGO_TARGET_DIR;
  process.env.CARGO_TARGET_DIR = root;
  t.after(async () => {
    if (previous === undefined) delete process.env.CARGO_TARGET_DIR;
    else process.env.CARGO_TARGET_DIR = previous;
    await rm(root, { recursive: true, force: true });
  });
  for (const entry of manifest.platforms.filter((entry) => entry.defaultVariant === "cuda")) {
    await writeFile(join(root, "native-build.json"), JSON.stringify({ target: entry.target, variant: "cuda" }));
    await assert.rejects(buildNative(entry, { runCommand(command) {
      if (command === "nvcc") return "release 13.1, V13.1.1";
      throw new Error("fallback build or linkage audit failed");
    } }), /fallback build or linkage audit failed/);
    await assert.rejects(existingNativeBuild(entry), /previous native build did not complete/);
    await assert.rejects(existingNativeBuild(entry, { cpuOnly: true }), /previous native build did not complete/);
    const build = await buildNative(entry, { runCommand(command) {
      if (command === "nvcc") return "release 13.1, V13.1.1";
    } });
    assert.deepEqual(await existingNativeBuild(entry), build);
  }
});

test("rejects platforms without a local release target", () => {
  assert.throws(
    () => selectPlatform(manifest, "freebsd", "x64"),
    /unsupported npm platform/,
  );
  assert.throws(
    () => selectPlatform(manifest, "win32", "ia32"),
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
