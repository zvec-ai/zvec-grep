"use strict";

const assert = require("node:assert/strict");
const { createHash } = require("node:crypto");
const {
  chmodSync,
  cpSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} = require("node:fs");
const { dirname, join } = require("node:path");
const { tmpdir } = require("node:os");
const test = require("node:test");
const { spawn, spawnSync } = require("node:child_process");
const { createServer } = require("node:http");

const { materialize, selectPlatform, verifyPayload, verifyInstallation } = require("./install.cjs");
const { coreIdentity, packageFilename, componentArchive, settings } = require("./plugins.cjs");

const entry = {
  target: "darwin-arm64",
  package: "@zvec/zvec-grep-darwin-arm64",
  os: "darwin",
  cpu: "arm64",
  binary: "zg",
};

function sha256(contents) {
  return createHash("sha256").update(contents).digest("hex");
}

function fixture() {
  const root = mkdtempSync(join(tmpdir(), "zvec-grep-npm-install-"));
  const metaRoot = join(root, "meta");
  const platformRoot = join(root, "platform");
  mkdirSync(join(metaRoot, "bin"), { recursive: true });
  mkdirSync(join(platformRoot, "bin"), { recursive: true });
  mkdirSync(join(platformRoot, "lib"), { recursive: true });
  const binary = Buffer.from("#!/bin/sh\necho 0.0.1\n");
  const library = Buffer.from("native-library-fixture\n");
  writeFileSync(join(metaRoot, "bin", "zg.exe"), "placeholder\n");
  writeFileSync(join(platformRoot, "bin", "zg"), binary);
  chmodSync(join(platformRoot, "bin", "zg"), 0o755);
  writeFileSync(join(platformRoot, "lib", "libbackend.dylib"), library);
  writeFileSync(join(platformRoot, "lib", "libfixture.dylib"), library);
  writeFileSync(
    join(platformRoot, "package.json"),
    JSON.stringify({ name: entry.package, version: "0.0.1" }),
  );
  writeFileSync(
    join(platformRoot, "checksums.json"),
    JSON.stringify({
      schemaVersion: 1,
      target: entry.target,
      files: {
        "bin/zg": { bytes: binary.length, sha256: sha256(binary) },
        "lib/libbackend.dylib": { bytes: library.length, sha256: sha256(library) },
        "lib/libfixture.dylib": { bytes: library.length, sha256: sha256(library) },
      },
    }),
  );
  return { root, metaRoot, platformRoot };
}

test("materializes a verified native package and replaces the placeholder last", () => {
  const paths = fixture();
  try {
    materialize({ ...paths, entry, expectedVersion: "0.0.1" });
    assert.match(readFileSync(join(paths.metaRoot, "bin", "zg.exe"), "utf8"), /0\.0\.1/);
    assert.equal(
      readFileSync(join(paths.metaRoot, "lib", "libbackend.dylib"), "utf8"),
      "native-library-fixture\n",
    );
    assert.equal(
      readFileSync(join(paths.metaRoot, "lib", "libfixture.dylib"), "utf8"),
      "native-library-fixture\n",
    );
    assert.notEqual(statSync(join(paths.metaRoot, "bin", "zg.exe")).mode & 0o111, 0);
  } finally {
    rmSync(paths.root, { recursive: true, force: true });
  }
});

test("rejects version and checksum mismatches without replacing the placeholder", () => {
  const paths = fixture();
  try {
    assert.throws(
      () => verifyPayload(paths.platformRoot, entry, "0.0.2"),
      /native package mismatch/,
    );
    writeFileSync(join(paths.platformRoot, "bin", "zg"), "corrupt\n");
    assert.throws(
      () => materialize({ ...paths, entry, expectedVersion: "0.0.1" }),
      /checksum mismatch/,
    );
    assert.equal(readFileSync(join(paths.metaRoot, "bin", "zg.exe"), "utf8"), "placeholder\n");
  } finally {
    rmSync(paths.root, { recursive: true, force: true });
  }
});

test("rejects native files that are not covered by the checksum manifest", () => {
  const paths = fixture();
  try {
    writeFileSync(join(paths.platformRoot, "bin", "untracked.dylib"), "untracked\n");
    assert.throws(
      () => verifyPayload(paths.platformRoot, entry, "0.0.1"),
      /do not match the checksum manifest/,
    );
  } finally {
    rmSync(paths.root, { recursive: true, force: true });
  }
});

test("selects one published platform and rejects unsupported libc", () => {
  assert.equal(selectPlatform([entry], "darwin", "arm64"), entry);
  assert.throws(
    () => selectPlatform([entry], "linux", "x64", { glibcVersionRuntime: "2.39" }),
    /does not publish/,
  );
});

test("selects the ARM64 Windows optional dependency from the release manifest", () => {
  const manifest = JSON.parse(readFileSync(join(__dirname, "..", "platforms.json"), "utf8"));
  const selected = selectPlatform(manifest.platforms, "win32", "arm64");
  assert.equal(selected.package, "@zvec/zvec-grep-win32-arm64-msvc");
  assert.equal(selected.binary, "zg.exe");
});

function componentFixture(target = "linux-x64-gnu") {
  const paths = fixture();
  const platform = require("../platforms.json").platforms.find((candidate) => candidate.target === target);
  const manifest = JSON.parse(readFileSync(join(paths.platformRoot, "checksums.json"), "utf8"));
  manifest.target = target;
  if (platform.binary !== "zg") {
    renameSync(join(paths.platformRoot, "bin/zg"), join(paths.platformRoot, "bin", platform.binary));
    manifest.files[`bin/${platform.binary}`] = manifest.files["bin/zg"];
    delete manifest.files["bin/zg"];
  }
  const originals = new Map();
  const pluginFiles = {};
  for (const file of new Set(platform.components.map((component) => component.file))) {
    const contents = Buffer.alloc(2 * 1024 * 1024 + 37);
    for (let i = 0; i < contents.length; i++) contents[i] = (i + file.length) % 251;
    originals.set(file, contents);
    pluginFiles[file] = { bytes: contents.length, sha256: sha256(contents) };
    const siblings = platform.components.filter((component) => component.file === file);
    for (const [index, component] of siblings.entries()) {
      const directory = join(paths.root, "component-sources", component.package);
      mkdirSync(join(directory, component.payload, ".."), { recursive: true });
      const bytes = contents.subarray(Math.floor(contents.length * index / siblings.length),
        Math.floor(contents.length * (index + 1) / siblings.length));
      writeFileSync(join(directory, component.payload), bytes);
      writeFileSync(join(directory, "package.json"), JSON.stringify({ name: component.package, version: "0.0.1" }));
      writeFileSync(join(directory, "checksums.json"), JSON.stringify({
        schemaVersion: 1, target, component: component.package,
        files: { [component.payload]: { bytes: bytes.length, sha256: sha256(bytes) } },
      }));
    }
  }
  writeFileSync(join(paths.platformRoot, "checksums.json"), JSON.stringify(manifest));
  writeFileSync(join(paths.platformRoot, "package.json"), JSON.stringify({
    name: platform.package, version: "0.0.1",
  }));
  return { ...paths, entry: platform, originals, pluginFiles };
}

for (const target of ["linux-x64-gnu", "win32-x64-msvc"]) {
  test(`assembles ${target} CUDA components into the unchanged bin/lib layout`, (t) => {
    const paths = optionalFixture(t, target);
    const options = { ...paths, config: { ...paths.config, mode: "cuda" } };
    materialize(options);
    for (const [file, contents] of paths.originals) {
      assert.deepEqual(readFileSync(join(paths.metaRoot, file)), contents);
    }
    assert.match(readFileSync(join(paths.metaRoot, "bin/zg.exe"), "utf8"), /0\.0\.1/);
    // Reinstallation also verifies and replaces an existing complete runtime.
    materialize(options);
  });
}

for (const failure of ["missing", "version", "corrupt", "swapped", "unexpected-manifest-file", "traversal"]) {
  test(`rejects ${failure} optional components without replacing the CPU installation`, (t) => {
    const paths = optionalFixture(t, "linux-x64-gnu", ({ source, component, paths }) => {
      const parts = paths.entry.components.filter((candidate) => candidate.payload.startsWith("parts/"));
      if (component !== parts[0]) return;
      const checksums = JSON.parse(readFileSync(join(source, "checksums.json"), "utf8"));
      if (failure === "version") writeFileSync(join(source, "package.json"), JSON.stringify({ name: component.package, version: "0.0.2" }));
      if (failure === "corrupt") writeFileSync(join(source, component.payload), "corrupt");
      if (failure === "unexpected-manifest-file") checksums.files["parts/untracked"] = { bytes: 0, sha256: sha256("") };
      if (failure === "traversal") checksums.files = { "parts/../../outside": checksums.files[component.payload] };
      if (failure === "swapped") {
        // Both the archive and fragment hashes are valid. Only the reconstructed
        // whole-library hash can detect a fragment in the wrong position.
        const wrong = readFileSync(join(paths.root, "component-sources", parts[1].package, parts[1].payload));
        writeFileSync(join(source, component.payload), wrong);
        checksums.files[component.payload] = { bytes: wrong.length, sha256: sha256(wrong) };
      }
      writeFileSync(join(source, "checksums.json"), JSON.stringify(checksums));
      if (failure === "swapped") return checksums.files[component.payload];
    });
    if (failure === "missing") rmSync(join(paths.config.directory, paths.plugins.components.at(-1).tarball));
    materialize(paths);
    const before = installedBaseSnapshot(paths);
    assert.throws(() => materialize({ ...paths, config: { ...paths.config, mode: "cuda" } }),
      /missing|mismatch|checksum|manifest|ENOENT/);
    assert.deepEqual(installedBaseSnapshot(paths), before);
    assert.deepEqual(verifyInstallation(paths).components, []);
    for (const file of paths.originals.keys()) assert.equal(existsSync(join(paths.metaRoot, file)), false);
  });
}

test("rejects the unpublished schema 2 without changing the existing installation", (t) => {
  const paths = fixture();
  t.after(() => rmSync(paths.root, { recursive: true, force: true }));
  materialize({ ...paths, entry, expectedVersion: "0.0.1" });
  const before = readFileSync(join(paths.metaRoot, "bin/zg.exe"));
  const checksums = JSON.parse(readFileSync(join(paths.platformRoot, "checksums.json")));
  checksums.schemaVersion = 2;
  writeFileSync(join(paths.platformRoot, "checksums.json"), JSON.stringify(checksums));
  assert.throws(() => materialize({ ...paths, entry, expectedVersion: "0.0.1" }), /invalid native checksum manifest/);
  assert.deepEqual(readFileSync(join(paths.metaRoot, "bin/zg.exe")), before);
});

function optionalFixture(t, target = "linux-x64-gnu", mutateComponent = () => {}) {
  const paths = componentFixture(target);
  t.after(() => rmSync(paths.root, { recursive: true, force: true }));
  const checksums = JSON.parse(readFileSync(join(paths.platformRoot, "checksums.json")));
  checksums.schemaVersion = 3;
  checksums.coreId = coreIdentity(paths.entry.target, "0.0.1", checksums.files);
  const directory = join(paths.root, "archives");
  mkdirSync(directory);
  const components = paths.entry.components.map((component, index) => {
    const source = join(paths.root, "component-sources", component.package);
    const checksum = JSON.parse(readFileSync(join(source, "checksums.json"))).files[component.payload];
    const pinnedChecksum = mutateComponent({ source, component, paths }) ?? checksum;
    const packing = join(paths.root, "packing", String(index));
    cpSync(source, join(packing, "package"), { recursive: true });
    const tarball = packageFilename(component.package, "0.0.1");
    const result = spawnSync("tar", ["-czf", join(directory, tarball), "-C", packing, "package"],
      { env: { ...process.env, COPYFILE_DISABLE: "1" } });
    assert.equal(result.status, 0);
    const bytes = readFileSync(join(directory, tarball));
    return { ...component, version: "0.0.1", tarball, archiveBytes: bytes.length,
      integrity: `sha512-${createHash("sha512").update(bytes).digest("base64")}`,
      checksum: pinnedChecksum };
  });
  // No installed component packages may be needed for CPU or later CUDA installation.
  rmSync(join(paths.root, "component-sources"), { recursive: true });
  const plugins = { schemaVersion: 1, target, version: "0.0.1", coreId: checksums.coreId, components, files: paths.pluginFiles };
  const raw = JSON.stringify(plugins);
  checksums.pluginsSha256 = sha256(raw);
  writeFileSync(join(paths.platformRoot, "plugins.json"), raw);
  writeFileSync(join(paths.platformRoot, "checksums.json"), JSON.stringify(checksums));
  writeFileSync(join(paths.platformRoot, "package.json"), JSON.stringify({ name: paths.entry.package, version: "0.0.1" }));
  return { ...paths, expectedVersion: "0.0.1", plugins,
    config: { mode: "cpu", directory, cache: join(paths.root, "cache"), offline: true } };
}

test("CPU installation needs no GPU detection or component requests", (t) => {
  const fixture = optionalFixture(t);
  materialize({ ...fixture, detectNvidia: () => assert.fail("must not probe GPU"),
    fetchArchive: () => assert.fail("must not request CUDA") });
  assert.deepEqual(verifyInstallation(fixture).components, []);
  for (const file of fixture.originals.keys()) assert.equal(existsSync(join(fixture.metaRoot, file)), false);
  assert.throws(() => verifyInstallation({ ...fixture, requireCuda: true }), /required CUDA plugins missing/);
});

test("automatic CPU-host install makes zero component requests", (t) => {
  const fixture = optionalFixture(t);
  materialize({ ...fixture, config: { ...fixture.config, mode: "auto" }, detectNvidia: () => false,
    fetchArchive: () => assert.fail("must not request CUDA") });
  assert.deepEqual(verifyInstallation(fixture).components, []);
});

for (const target of ["linux-x64-gnu", "win32-x64-msvc"]) {
  test(`CPU then offline CUDA installation preserves the same binary and layout: ${target}`, (t) => {
    const fixture = optionalFixture(t, target);
    materialize(fixture);
    const binary = readFileSync(join(fixture.metaRoot, "bin/zg.exe"));
    materialize({ ...fixture, config: { ...fixture.config, mode: "cuda" },
      detectNvidia: () => assert.fail("forced install must work on a GPU-free image builder") });
    assert.deepEqual(readFileSync(join(fixture.metaRoot, "bin/zg.exe")), binary);
    for (const [file, content] of fixture.originals) assert.deepEqual(readFileSync(join(fixture.metaRoot, file)), content);
    assert.equal(verifyInstallation({ ...fixture, requireCuda: true }).components.length, fixture.plugins.components.length);
    materialize({ ...fixture, config: { ...fixture.config, mode: "cuda" }, fetchArchive: () => assert.fail("idempotent rebuild") });
  });
}

for (const mode of ["auto", "cuda"]) {
  for (const failure of ["missing", "corrupt"]) {
    test(`${mode} ${failure} plugin attempt leaves a complete CPU installation`, (t) => {
      const fixture = optionalFixture(t);
      materialize(fixture);
      const base = installedBaseSnapshot(fixture);
      const path = join(fixture.config.directory, fixture.plugins.components.at(-1).tarball);
      const archive = readFileSync(path);
      if (failure === "missing") rmSync(path); else writeFileSync(path, "corrupt");
      const action = () => materialize({ ...fixture, config: { ...fixture.config, mode }, detectNvidia: () => true });
      if (mode === "cuda") assert.throws(action, /ENOENT|integrity mismatch/); else action();
      assert.deepEqual(installedBaseSnapshot(fixture), base);
      assert.deepEqual(verifyInstallation(fixture).components, []);
      for (const file of fixture.originals.keys()) assert.equal(existsSync(join(fixture.metaRoot, file)), false);
      writeFileSync(path, archive);
      materialize({ ...fixture, config: { ...fixture.config, mode: "cuda" } });
      assert.equal(verifyInstallation({ ...fixture, requireCuda: true }).components.length, fixture.plugins.components.length);
    });
  }
}

function installedBaseSnapshot(fixture) {
  const checksums = JSON.parse(readFileSync(join(fixture.platformRoot, "checksums.json")));
  return Object.fromEntries(Object.keys(checksums.files).map((file) => {
    const installed = file === `bin/${fixture.entry.binary}` ? "bin/zg.exe" : file;
    return [installed, readFileSync(join(fixture.metaRoot, installed))];
  }));
}

function interruptedPluginAttempt(fixture, phase, kill, mode = "cuda") {
  // Instrument only this child process's filesystem calls. Exercise the real
  // installer without adding production failpoints or extra helper scripts.
  const script = `
    const fs = require('node:fs');
    const { join } = require('node:path');
    const { options, installer, phase, kill, mode } = JSON.parse(process.argv[1]);
    let injected = false;
    function interrupt() {
      injected = true;
      fs.writeSync(1, 'interrupted:' + phase + '\\n');
      if (kill) process.kill(process.pid, 'SIGKILL');
      throw new Error('injected plugin failure: ' + phase);
    }
    const rename = fs.renameSync;
    const files = JSON.parse(fs.readFileSync(join(options.platformRoot, 'plugins.json'))).files;
    const destinations = Object.keys(files).map(file => join(options.metaRoot, file));
    let activated = 0;
    fs.renameSync = (source, destination) => {
      if (!injected && phase === 'activation' && destinations.includes(destination) && ++activated === 2) interrupt();
      if (!injected && phase === 'receipt' && destination === join(options.metaRoot, 'native-install.json')) interrupt();
      return rename(source, destination);
    };
    const { materialize } = require(installer);
    const { componentArchive } = require(join(require('node:path').dirname(installer), 'plugins.cjs'));
    const config = { ...options.config, mode };
    const fetchArchive = phase === 'download' ? (component, manifest) =>
      componentArchive(component, manifest, { ...config, directory: undefined, offline: false }, (item, directory) => {
        fs.writeFileSync(join(directory, item.tarball), 'incomplete download');
        interrupt();
      }) : undefined;
    materialize({ ...options, config, fetchArchive, detectNvidia: () => true });
  `;
  return spawnSync(process.execPath, ["-e", script, JSON.stringify({
    options: { metaRoot: fixture.metaRoot, platformRoot: fixture.platformRoot,
      entry: fixture.entry, expectedVersion: fixture.expectedVersion, config: fixture.config },
    installer: join(__dirname, "install.cjs"), phase, kill, mode,
  })], { encoding: "utf8", timeout: 15000 });
}

for (const target of ["linux-x64-gnu", "win32-x64-msvc"]) {
  for (const phase of ["download", "activation", "receipt"]) {
    test(`killed CUDA ${phase} preserves the CPU payload and permits retry: ${target}`, (t) => {
      const fixture = optionalFixture(t, target);
      materialize(fixture);
      const base = installedBaseSnapshot(fixture);
      const result = interruptedPluginAttempt(fixture, phase, true);
      assert.ifError(result.error);
      assert.match(result.stdout, new RegExp(`interrupted:${phase}`), result.stderr);
      assert.notEqual(result.status, 0);
      assert.equal(existsSync(join(fixture.metaRoot, ".native-install.lock")), true);
      assert.deepEqual(installedBaseSnapshot(fixture), base);
      assert.deepEqual(JSON.parse(readFileSync(join(fixture.metaRoot, "native-install.json"))).components, []);
      // Even if activation was interrupted, no truncated or unverified library
      // becomes visible. CPU bytes remain intact before any recovery is run.
      for (const [file, content] of fixture.originals) {
        if (existsSync(join(fixture.metaRoot, file))) assert.deepEqual(readFileSync(join(fixture.metaRoot, file)), content);
      }
      const config = { ...fixture.config, mode: "cuda",
        ...(phase === "download" ? { directory: undefined, offline: false } : {}) };
      const fetchArchive = phase === "download" ? (component, manifest, settings) =>
        componentArchive(component, manifest, settings, (item, directory) => {
          const path = join(directory, item.tarball);
          cpSync(join(fixture.config.directory, item.tarball), path);
          return path;
        }) : undefined;
      // Retry CUDA directly, without an intermediate CPU repair command. A
      // killed download also retries the same cache key, not a different source.
      materialize({ ...fixture, config, fetchArchive });
      assert.equal(verifyInstallation({ ...fixture, requireCuda: true }).components.length, fixture.plugins.components.length);
      assert.deepEqual(installedBaseSnapshot(fixture), base);
      assert.equal(existsSync(join(fixture.metaRoot, ".native-install.lock")), false);
    });
  }
}

for (const mode of ["auto", "cuda"]) {
  for (const phase of ["activation", "receipt"]) {
    test(`${mode} CUDA ${phase} error rolls back plugins without replacing CPU files`, (t) => {
      const fixture = optionalFixture(t);
      materialize(fixture);
      const base = installedBaseSnapshot(fixture);
      const binary = statSync(join(fixture.metaRoot, "bin/zg.exe"));
      const result = interruptedPluginAttempt(fixture, phase, false, mode);
      assert.ifError(result.error);
      assert.match(result.stdout, new RegExp(`interrupted:${phase}`), result.stderr);
      if (mode === "auto") assert.equal(result.status, 0, result.stderr);
      else assert.notEqual(result.status, 0);
      assert.match(result.stderr, /injected plugin failure/);
      assert.deepEqual(verifyInstallation(fixture).components, []);
      assert.deepEqual(installedBaseSnapshot(fixture), base);
      assert.equal(statSync(join(fixture.metaRoot, "bin/zg.exe")).ino, binary.ino);
      materialize({ ...fixture, config: { ...fixture.config, mode: "cuda" } });
      assert.equal(verifyInstallation({ ...fixture, requireCuda: true }).components.length, fixture.plugins.components.length);
    });
  }
}

test("corrupt base is fatal even in automatic mode", (t) => {
  const fixture = optionalFixture(t);
  writeFileSync(join(fixture.platformRoot, "bin/zg"), "corrupted");
  assert.throws(() => materialize({ ...fixture, config: { ...fixture.config, mode: "auto" } }), /checksum mismatch/);
  assert.equal(readFileSync(join(fixture.metaRoot, "bin/zg.exe"), "utf8"), "placeholder\n");
});

test("plugin core/ABI and manifest changes are rejected before installation", (t) => {
  const fixture = optionalFixture(t);
  const path = join(fixture.platformRoot, "plugins.json");
  writeFileSync(path, JSON.stringify({ ...fixture.plugins, coreId: "0".repeat(64) }));
  assert.throws(() => materialize(fixture), /manifest binding/);
});

test("content-addressed archives support offline reuse but never another core", (t) => {
  const fixture = optionalFixture(t);
  const component = fixture.plugins.components[0];
  const config = { ...fixture.config, directory: undefined, offline: false };
  let requests = 0;
  const fetch = (_, directory) => {
    requests++;
    const path = join(directory, component.tarball);
    cpSync(join(fixture.config.directory, component.tarball), path);
    return path;
  };
  componentArchive(component, fixture.plugins, config, fetch);
  componentArchive(component, fixture.plugins, { ...config, offline: true }, fetch);
  assert.equal(requests, 1);
  assert.throws(() => componentArchive(component, { ...fixture.plugins, coreId: "a".repeat(64) },
    { ...config, offline: true }, fetch), /offline plugin cache miss/);
});

test("concurrent installation is rejected without touching the CPU runtime", (t) => {
  const fixture = optionalFixture(t);
  materialize(fixture);
  writeFileSync(join(fixture.metaRoot, ".native-install.lock"), String(process.pid));
  assert.throws(() => materialize(fixture), /installation is in progress/);
  assert.deepEqual(verifyInstallation(fixture).components, []);
});

function staleLockRetry(t, fixture, pauseWriter) {
  const installer = join(__dirname, "install.cjs");
  const options = { metaRoot: fixture.metaRoot, platformRoot: fixture.platformRoot,
    entry: fixture.entry, expectedVersion: fixture.expectedVersion,
    config: { ...fixture.config, mode: "cuda" } };
  const source = `
    const fs = require('node:fs');
    const [installer, serialized, pauseWriter] = process.argv.slice(1);
    const options = JSON.parse(serialized);
    const signal = name => fs.writeSync(1, name + '\\n');
    const resume = () => {
      if (fs.readSync(0, Buffer.alloc(1), 0, 1, null) !== 1) throw new Error('missing test barrier');
    };
    const kill = process.kill;
    let observed = false;
    process.kill = (pid, signalNumber) => {
      if (pid === 2147483647) {
        if (!observed) { observed = true; signal('observed-stale'); resume(); }
        throw Object.assign(new Error('dead test owner'), { code: 'ESRCH' });
      }
      return kill(pid, signalNumber);
    };
    const { materialize } = require(installer);
    const { componentArchive } = require(require('node:path').join(require('node:path').dirname(installer), 'plugins.cjs'));
    let entered = false;
    materialize({ ...options, fetchArchive: (...args) => {
      if (!entered) { entered = true; signal('entered'); if (pauseWriter === 'true') resume(); }
      return componentArchive(...args);
    } });
  `;
  const child = spawn(process.execPath, ["-e", source, installer, JSON.stringify(options), String(pauseWriter)],
    { stdio: ["pipe", "pipe", "pipe"] });
  t.after(() => { if (child.exitCode === null) child.kill(); });
  let stdout = "", stderr = "", ended = false;
  const waiting = new Map();
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
    for (const [marker, waiter] of waiting) {
      if (stdout.includes(`${marker}\n`)) { waiting.delete(marker); waiter.resolve(); }
    }
  });
  child.stderr.on("data", (chunk) => { stderr += chunk; });
  const done = new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("close", (code) => {
      ended = true;
      for (const waiter of waiting.values()) waiter.reject(new Error(`retry exited before barrier: ${stderr}`));
      resolve({ code, stdout, stderr });
    });
  });
  return { child, done, waitFor(marker) {
    if (stdout.includes(`${marker}\n`)) return Promise.resolve();
    if (ended) return Promise.reject(new Error(`retry exited before ${marker}: ${stderr}`));
    return new Promise((resolve, reject) => waiting.set(marker, { resolve, reject }));
  } };
}

for (const target of ["linux-x64-gnu", "win32-x64-msvc"]) {
  test(`two retries observing one stale lock cannot both activate plugins: ${target}`, { timeout: 60000 }, async (t) => {
    const fixture = optionalFixture(t, target);
    materialize(fixture);
    const before = installedBaseSnapshot(fixture);
    const lock = join(fixture.metaRoot, ".native-install.lock");
    writeFileSync(lock, "2147483647");
    const first = staleLockRetry(t, fixture, true);
    const second = staleLockRetry(t, fixture, false);
    await Promise.all([first.waitFor("observed-stale"), second.waitFor("observed-stale")]);
    first.child.stdin.write("1");
    await first.waitFor("entered"); // The first writer holds the replacement lock.
    const owner = readFileSync(lock, "utf8");
    const record = JSON.parse(owner);
    assert.equal(typeof record === "number" ? record : record.pid, first.child.pid);
    second.child.stdin.write("1"); // Resume the stale observation AFTER replacement.
    const rejected = await second.done;
    assert.notEqual(rejected.code, 0);
    assert.match(rejected.stderr, /lock owner changed/);
    assert.doesNotMatch(rejected.stdout, /entered/);
    assert.equal(readFileSync(lock, "utf8"), owner);
    assert.deepEqual(installedBaseSnapshot(fixture), before);
    assert.deepEqual(JSON.parse(readFileSync(join(fixture.metaRoot, "native-install.json"))).components, []);
    first.child.stdin.write("1");
    const completed = await first.done;
    assert.equal(completed.code, 0, completed.stderr);
    assert.equal(verifyInstallation({ ...fixture, requireCuda: true }).components.length, fixture.plugins.components.length);
    assert.deepEqual(installedBaseSnapshot(fixture), before);
    assert.equal(existsSync(lock), false);
    assert.equal(existsSync(`${lock}.recovery`), false);
  });
}

test("release never deletes a replacement owner's lock", (t) => {
  const fixture = optionalFixture(t);
  materialize(fixture);
  const lock = join(fixture.metaRoot, ".native-install.lock");
  const replacement = JSON.stringify({ pid: process.pid, token: "different-owner" });
  materialize({ ...fixture, config: { ...fixture.config, mode: "cuda" }, fetchArchive: (...args) => {
    writeFileSync(lock, replacement);
    return componentArchive(...args);
  } });
  assert.equal(readFileSync(lock, "utf8"), replacement);
  assert.equal(verifyInstallation({ ...fixture, requireCuda: true }).components.length, fixture.plugins.components.length);
});

test("an interrupted recovery guard fails closed without changing the CPU installation", (t) => {
  const fixture = optionalFixture(t);
  materialize(fixture);
  const before = installedBaseSnapshot(fixture);
  const dead = spawnSync(process.execPath, ["-e", ""]);
  assert.equal(dead.status, 0);
  const lock = join(fixture.metaRoot, ".native-install.lock");
  writeFileSync(lock, String(dead.pid));
  mkdirSync(`${lock}.recovery`);
  assert.throws(() => materialize(fixture), /inspect \.native-install.lock.recovery/);
  assert.equal(readFileSync(lock, "utf8"), String(dead.pid));
  assert.deepEqual(installedBaseSnapshot(fixture), before);
});

test("relocating the completed install does not depend on its cache", (t) => {
  const fixture = optionalFixture(t);
  materialize({ ...fixture, config: { ...fixture.config, mode: "cuda" } });
  const relocated = join(fixture.root, "relocated");
  renameSync(fixture.metaRoot, relocated);
  rmSync(fixture.config.directory, { recursive: true });
  assert.equal(verifyInstallation({ ...fixture, metaRoot: relocated, requireCuda: true }).components.length, 4);
});

test("installation settings reject unknown modes and honor npm offline", () => {
  assert.throws(() => settings({ ZG_INSTALL_ACCELERATOR: "gpu" }), /must be auto, cpu or cuda/);
  assert.equal(settings({ npm_config_offline: "true" }).offline, true);
});

test("real npm install/rebuild supports CPU bootstrap, offline plugins and pinned registry downloads", async (t) => {
  const fixture = optionalFixture(t, process.platform === "win32" ? "win32-x64-msvc" : "linux-x64-gnu");
  const meta = join(fixture.root, "meta-source", "package");
  mkdirSync(join(meta, "bin"), { recursive: true });
  writeFileSync(join(meta, "bin/zg.exe"), "uninstalled placeholder");
  for (const file of ["install.cjs", "plugins.cjs"]) cpSync(join(__dirname, file), join(meta, file));
  writeFileSync(join(meta, "platforms.json"), JSON.stringify([{ ...fixture.entry, os: process.platform, cpu: process.arch,
    libc: process.platform === "linux" ? "glibc" : undefined }]));
  writeFileSync(join(meta, "package.json"), JSON.stringify({ name: "@zvec/zvec-grep", version: "0.0.1",
    scripts: { postinstall: "node install.cjs" }, optionalDependencies: { [fixture.entry.package]: "0.0.1" } }));
  const base = join(fixture.root, "base-source", "package");
  cpSync(fixture.platformRoot, base, { recursive: true });
  for (const [source, name] of [[meta, "meta"], [base, "base"]]) {
    const result = spawnSync("tar", ["-czf", join(fixture.root, `${name}.tgz`), "-C", dirname(source), "package"],
      { env: { ...process.env, COPYFILE_DISABLE: "1" } });
    assert.equal(result.status, 0);
  }
  const prefix = join(fixture.root, "prefix");
  const installedMeta = join(prefix, ...(process.platform === "win32" ? [] : ["lib"]), "node_modules/@zvec/zvec-grep");
  const installedBase = join(prefix, ...(process.platform === "win32" ? [] : ["lib"]), "node_modules", fixture.entry.package);
  const npm = (args, mode) => {
    const npmCli = process.env.npm_execpath || join(dirname(process.execPath), "node_modules/npm/bin/npm-cli.js");
    const windows = process.platform === "win32";
    const result = spawnSync(windows ? process.execPath : "npm", windows ? [npmCli, ...args] : args, {
      encoding: "utf8", timeout: 60000, cwd: fixture.root,
      env: { ...process.env, npm_config_cache: join(fixture.root, "npm-cache"),
        npm_config_offline: "true", ZG_INSTALL_ACCELERATOR: mode, ZG_PLUGIN_DIR: fixture.config.directory,
        ZG_PLUGIN_CACHE: fixture.config.cache, ZG_INSTALL_OFFLINE: "1" },
    });
    assert.equal(result.status, 0, `${result.error?.message || ""} ${result.signal || ""}\n${result.stdout}${result.stderr}`);
  };
  npm(["install", "-g", "--prefix", prefix, "--offline", "--ignore-scripts", "--no-audit", "--no-fund",
    join(fixture.root, "base.tgz"), join(fixture.root, "meta.tgz")], "cpu");
  assert.equal(readFileSync(join(installedMeta, "bin/zg.exe"), "utf8"), "uninstalled placeholder");
  const rebuild = ["rebuild", "-g", "--prefix", prefix, "@zvec/zvec-grep", "--ignore-scripts=false",
    "--allow-scripts=@zvec/zvec-grep", "--foreground-scripts"];
  npm(rebuild, "cpu");
  const options = { ...fixture, metaRoot: installedMeta, platformRoot: installedBase };
  assert.deepEqual(verifyInstallation(options).components, []);
  npm(rebuild, "cuda");
  assert.equal(verifyInstallation({ ...options, requireCuda: true }).components.length, fixture.plugins.components.length);
  // Downloaded components never became npm dependencies or project lock entries.
  for (const component of fixture.plugins.components) assert.equal(existsSync(join(dirname(installedBase), component.package.split("/").at(-1))), false);

  // Serve only the pinned test archives. The online path must use this registry,
  // never a public/published version, and it must not execute component scripts.
  const requests = [];
  const server = createServer((request, response) => {
    requests.push(request.url);
    const path = decodeURIComponent(request.url);
    const component = fixture.plugins.components.find((c) => path === `/${c.package}` || path === `/tarballs/${c.tarball}`);
    if (!component) { response.writeHead(404); response.end(); return; }
    if (path.startsWith("/tarballs/")) {
      response.end(readFileSync(join(fixture.config.directory, component.tarball)));
      return;
    }
    response.setHeader("Content-Type", "application/json");
    response.end(JSON.stringify({ name: component.package, "dist-tags": { latest: "0.0.1" }, versions: {
      "0.0.1": { name: component.package, version: "0.0.1", dist: {
        tarball: `http://127.0.0.1:${server.address().port}/tarballs/${component.tarball}`, integrity: component.integrity,
      } },
    } }));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  const onlinePrefix = join(fixture.root, "online-prefix");
  npm(["install", "-g", "--prefix", onlinePrefix, "--offline", "--ignore-scripts=false", "--allow-scripts=@zvec/zvec-grep",
    "--no-audit", "--no-fund", join(fixture.root, "base.tgz"), join(fixture.root, "meta.tgz")], "cpu");
  assert.equal(requests.length, 0);
  const npmCli = process.env.npm_execpath || join(dirname(process.execPath), "node_modules/npm/bin/npm-cli.js");
  const args = rebuild.map((arg) => arg === prefix ? onlinePrefix : arg);
  await new Promise((resolve, reject) => {
    const child = spawn(process.platform === "win32" ? process.execPath : "npm",
      process.platform === "win32" ? [npmCli, ...args] : args, {
        cwd: fixture.root, timeout: 60000,
        env: { ...process.env, npm_config_registry: `http://127.0.0.1:${server.address().port}`,
          npm_config_cache: join(fixture.root, "online-npm-cache"), npm_config_offline: "false",
          ZG_INSTALL_ACCELERATOR: "cuda", ZG_INSTALL_OFFLINE: "0", ZG_PLUGIN_DIR: "",
          ZG_PLUGIN_CACHE: join(fixture.root, "online-plugin-cache") },
      });
    let output = "";
    child.stdout.on("data", (data) => { output += data; });
    child.stderr.on("data", (data) => { output += data; });
    child.on("error", reject);
    child.on("close", (code) => code === 0 ? resolve() : reject(new Error(output)));
  });
  assert.equal(requests.filter((path) => path.startsWith("/tarballs/")).length, fixture.plugins.components.length);
  assert.equal(verifyInstallation({ ...options, metaRoot: installedMeta.replace(prefix, onlinePrefix),
    platformRoot: installedBase.replace(prefix, onlinePrefix), requireCuda: true }).components.length,
  fixture.plugins.components.length);
});
