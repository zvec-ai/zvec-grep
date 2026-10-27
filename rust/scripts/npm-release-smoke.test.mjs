import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { copyFile, cp, mkdir, mkdtemp, readFile, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { defaultReleaseVariant, loadReleaseManifest, nativePackageEntries, run, selectPlatform, sha256File } from "./npm-package-core.mjs";
import { installArtifactRelease, rebuildArtifactRelease, installedPackageRoot, rollbackVersion, smokeArtifactRelease, smokeRegistryRelease, verifyInstalledRelease } from "./npm-release-smoke.mjs";
import { materialize } from "../npm/meta/install.cjs";
import { coreIdentity, detectNvidia } from "../npm/meta/plugins.cjs";

const manifest = loadReleaseManifest();
const release = {
  version: "0.0.1",
  packages: [
    {
      name: manifest.metaPackage,
      version: "0.0.1",
      integrity: "sha512-candidate",
    },
  ],
};

async function artifactFixture(t, withComponents = false, componentTarget = "linux-x64-gnu") {
  const root = await mkdtemp(join(tmpdir(), "zg-artifact-smoke-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const entry = { ...selectPlatform(manifest, process.platform, process.arch,
    process.report?.getReport().header ?? {}) };
  if (withComponents) {
    const template = manifest.platforms.find((platform) => platform.target === componentTarget);
    entry.components = template.components.map((component) => ({
      ...component, package: component.package.replace(template.package, entry.package),
    }));
  } else {
    delete entry.components;
    delete entry.defaultVariant;
  }
  const fixtureManifest = { ...manifest, platforms: manifest.platforms.map((platform) =>
    platform.target === entry.target ? entry : platform) };
  const candidate = { schemaVersion: 1, version: "0.0.1", packages: [] };
  for (const { name, target } of [...nativePackageEntries(entry), { name: manifest.metaPackage, target: "meta" }]) {
    const file = `${name.replace(/^@/, "").replaceAll("/", "-")}-${candidate.version}.tgz`;
    await writeFile(join(root, file), `candidate artifact for ${name}`);
    candidate.packages.push({ name, target, version: candidate.version, file,
      sha256: await sha256File(join(root, file)) });
  }
  return { manifest: fixtureManifest, release: candidate, tarballRoot: root, prefixRoot: join(root, "install"), entry };
}

async function installedArtifact(fixture, prefix, variant = defaultReleaseVariant(fixture.entry)) {
  const directory = installedPackageRoot(prefix, fixture.entry.package);
  const metaRoot = installedPackageRoot(prefix, fixture.manifest.metaPackage);
  const components = fixture.entry.components ?? [];
  const checksum = (content) => ({ bytes: Buffer.byteLength(content), sha256: createHash("sha256").update(content).digest("hex") });
  const write = async (root, file, content) => {
    await mkdir(dirname(join(root, file)), { recursive: true });
    await writeFile(join(root, file), content);
  };
  const binary = `bin/${fixture.entry.binary}`;
  await write(directory, binary, "fixture-executable");
  await write(directory, "lib/base-library", "base");
  const files = { [binary]: checksum("fixture-executable"), "lib/base-library": checksum("base") };
  const external = new Map();
  const pinned = [];
  for (const component of components) {
    const root = installedPackageRoot(prefix, component.package);
    const content = `fixture component ${component.package}`;
    await write(root, component.payload, content);
    await write(root, "package.json", JSON.stringify({ name: component.package, version: fixture.release.version }));
    await write(root, "checksums.json", JSON.stringify({ schemaVersion: 1, target: fixture.entry.target,
      component: component.package, files: { [component.payload]: checksum(content) } }));
    external.set(component.file, (external.get(component.file) ?? "") + content);
    const pkg = fixture.release.packages.find((pkg) => pkg.name === component.package);
    await packInstalledArtifact(fixture, prefix, pkg);
    pinned.push({ ...component, version: pkg.version, tarball: pkg.file, archiveBytes: pkg.bytes,
      integrity: pkg.integrity, checksum: checksum(content) });
    await rm(root, { recursive: true });
  }
  const checksums = { schemaVersion: components.length ? 3 : 1, target: fixture.entry.target, files };
  if (components.length) {
    checksums.coreId = coreIdentity(fixture.entry.target, fixture.release.version, files);
    const plugins = JSON.stringify({ schemaVersion: 1, target: fixture.entry.target,
      version: fixture.release.version, coreId: checksums.coreId, components: pinned,
      files: Object.fromEntries([...external].map(([file, content]) => [file, checksum(content)])) });
    checksums.pluginsSha256 = checksum(plugins).sha256;
    await write(directory, "plugins.json", plugins);
  }
  await write(directory, "checksums.json", JSON.stringify(checksums));
  await writeFile(join(directory, "package.json"), JSON.stringify({
    name: fixture.entry.package, version: fixture.release.version, zvecGrep: { variant },
  }));
  await write(metaRoot, "package.json", JSON.stringify({ name: fixture.manifest.metaPackage, version: fixture.release.version }));
  materialize({ metaRoot, platformRoot: directory, entry: fixture.entry, expectedVersion: fixture.release.version,
    config: { mode: components.length ? "cuda" : "cpu", directory: fixture.tarballRoot, offline: true } });
}

async function packInstalledArtifact(fixture, prefix, pkg) {
  const staging = join(fixture.tarballRoot, "packing", pkg.name.replaceAll("/", "-"));
  await cp(installedPackageRoot(prefix, pkg.name), join(staging, "package"), { recursive: true });
  run("tar", ["-czf", join(fixture.tarballRoot, pkg.file), "-C", staging, "package"],
    { env: { ...process.env, COPYFILE_DISABLE: "1" } });
  const bytes = await readFile(join(fixture.tarballRoot, pkg.file));
  Object.assign(pkg, { sha256: createHash("sha256").update(bytes).digest("hex"),
    integrity: `sha512-${createHash("sha512").update(bytes).digest("base64")}`, bytes: bytes.length });
}

test("artifact smoke installs the exact local tarballs offline and probes the installed package", async (t) => {
  const fixture = await artifactFixture(t);
  let installs = 0;
  let probes = 0;
  const result = await smokeArtifactRelease({
    ...fixture,
    runNpm: async (command, args) => {
      assert.equal(command, "npm");
      assert.equal(args[0], "install");
      assert.ok(args.includes("--offline"));
      assert.deepEqual(args.slice(-fixture.release.packages.length), fixture.release.packages.map((pkg) => join(fixture.tarballRoot, pkg.file)));
      assert.ok(args.includes(`--allow-scripts=${args.at(-1)}`));
      await installedArtifact(fixture, args[args.indexOf("--prefix") + 1]);
      installs++;
    },
    probe: async ({ entry, version, prefix }) => {
      assert.equal(entry.target, fixture.entry.target);
      assert.equal(version, fixture.release.version);
      assert.equal(prefix, join(fixture.prefixRoot, "prefix"));
      probes++;
    },
  });
  assert.equal(installs, 1);
  assert.equal(probes, 1);
  assert.equal(result.bundle, installedPackageRoot(result.prefix, manifest.metaPackage));
  const report = JSON.parse(await readFile(join(fixture.prefixRoot, "installation.json"), "utf8"));
  assert.deepEqual(report.packages, fixture.release.packages);
  assert.equal(report.bundle, result.bundle);
});

test("the shared installer uses real offline npm postinstall without running the native executable", async (t) => {
  const fixture = await artifactFixture(t);
  const source = join(fixture.tarballRoot, "source");
  await installedArtifact(fixture, source);
  const metaRoot = installedPackageRoot(source, manifest.metaPackage);
  await rm(join(metaRoot, "lib"), { recursive: true });
  await writeFile(join(metaRoot, "bin/zg.exe"), "uninstalled placeholder");
  await copyFile(new URL("../npm/meta/install.cjs", import.meta.url), join(metaRoot, "install.cjs"));
  await copyFile(new URL("../npm/meta/plugins.cjs", import.meta.url), join(metaRoot, "plugins.cjs"));
  await writeFile(join(metaRoot, "platforms.json"), JSON.stringify([fixture.entry]));
  await writeFile(join(metaRoot, "package.json"), JSON.stringify({ name: manifest.metaPackage,
    version: fixture.release.version, scripts: { postinstall: "node install.cjs" },
    optionalDependencies: { [fixture.entry.package]: fixture.release.version } }));
  for (const pkg of fixture.release.packages) {
    const staging = join(fixture.tarballRoot, "packing", pkg.name.replaceAll("/", "-"));
    await cp(installedPackageRoot(source, pkg.name), join(staging, "package"), { recursive: true });
    run("tar", ["-czf", join(fixture.tarballRoot, pkg.file), "-C", staging, "package"],
      { env: { ...process.env, COPYFILE_DISABLE: "1" } });
    pkg.sha256 = await sha256File(join(fixture.tarballRoot, pkg.file));
  }
  const result = await installArtifactRelease(fixture);
  // This is deliberately not an executable: installation must not invoke it.
  assert.equal(await readFile(join(result.bundle, "bin/zg.exe"), "utf8"), "fixture-executable");
  const report = JSON.parse(await readFile(join(fixture.prefixRoot, "installation.json"), "utf8"));
  assert.deepEqual(report.components, []);
  for (const pkg of fixture.release.packages) assert.equal(await sha256File(join(fixture.tarballRoot, pkg.file)), pkg.sha256);
});

// Tiny archive payloads, but the real metadata, postinstall, npm install and npm
// rebuild protocol. CI uses this same helper with the compiled release artifacts.
async function optionalArtifactFixture(t, componentTarget) {
  const fixture = await artifactFixture(t, true, componentTarget);
  fixture.release.commit = "fixture-commit";
  const source = join(fixture.tarballRoot, "source");
  await installedArtifact(fixture, source);
  const metaRoot = installedPackageRoot(source, manifest.metaPackage);
  await rm(join(metaRoot, "lib"), { recursive: true });
  await rm(join(metaRoot, "native-install.json"));
  await writeFile(join(metaRoot, "bin/zg.exe"), "uninstalled placeholder");
  for (const file of ["install.cjs", "plugins.cjs"]) {
    await copyFile(new URL(`../npm/meta/${file}`, import.meta.url), join(metaRoot, file));
  }
  await writeFile(join(metaRoot, "platforms.json"), JSON.stringify([fixture.entry]));
  await writeFile(join(metaRoot, "package.json"), JSON.stringify({ name: manifest.metaPackage,
    version: fixture.release.version, scripts: { postinstall: "node install.cjs" },
    optionalDependencies: { [fixture.entry.package]: fixture.release.version } }));
  for (const name of [fixture.entry.package, manifest.metaPackage]) {
    await packInstalledArtifact(fixture, source, fixture.release.packages.find((pkg) => pkg.name === name));
  }
  return fixture;
}

for (const target of ["linux-x64-gnu", "win32-x64-msvc"]) {
  test(`default auto artifact smoke accepts real CPU postinstall, but GPU expectations reject it: ${target}`, async (t) => {
    if (detectNvidia()) return t.skip("default auto CPU installation requires a CPU host");
    const fixture = await optionalArtifactFixture(t, target);
    let probes = 0;
    await smokeArtifactRelease({ ...fixture, probe: async ({ prefix }) => {
      const installation = await verifyInstalledRelease({ ...fixture, version: fixture.release.version, prefix });
      assert.deepEqual(installation.components, []);
      probes++;
    } });
    assert.equal(probes, 1);
    const report = JSON.parse(await readFile(join(fixture.prefixRoot, "installation.json"), "utf8"));
    assert.equal(report.accelerator, "auto");
    assert.equal(report.expectCuda, false);
    await assert.rejects(smokeArtifactRelease({ ...fixture, expectCuda: true,
      probe: () => assert.fail("GPU CI must reject CPU fallback before inference"),
    }), /required CUDA plugins missing/);
  });

  test(`real npm default CPU install makes zero CUDA requests, then rebuild preserves the installation: ${target}`, async (t) => {
    if (detectNvidia()) return t.skip("default auto CPU installation requires a CPU host");
    const fixture = await optionalArtifactFixture(t, target);
    const installed = await installArtifactRelease({ ...fixture, expectCpu: true });
    const before = JSON.parse(await readFile(join(fixture.prefixRoot, "installation.json"), "utf8"));
    assert.equal(before.accelerator, "auto");
    assert.deepEqual(before.components, []);
    const cpuDownloads = JSON.parse(await readFile(join(fixture.prefixRoot, "cpu-downloads.json"), "utf8"));
    assert.equal(cpuDownloads.cudaRequests, 0);
    await writeFile(join(installed.bundle, "same-installation"), "must survive rebuild");
    const rebuilt = await rebuildArtifactRelease(fixture);
    assert.equal(rebuilt.bundle, installed.bundle);
    assert.equal(await readFile(join(rebuilt.bundle, "same-installation"), "utf8"), "must survive rebuild");
    assert.equal(await readFile(join(rebuilt.bundle, "bin/zg.exe"), "utf8"), "fixture-executable");
    const after = JSON.parse(await readFile(join(fixture.prefixRoot, "installation.json"), "utf8"));
    assert.equal(after.operation, "rebuild");
    assert.equal(after.coreId, before.coreId);
    assert.equal(after.previousCoreId, before.coreId);
    assert.equal(after.components.length, fixture.entry.components.length);
    const downloads = JSON.parse(await readFile(join(fixture.prefixRoot, "rebuild-downloads.json"), "utf8"));
    assert.equal(downloads.requests.filter((request) => request.path.startsWith("/tarballs/")).length,
      fixture.entry.components.length);
    await assert.rejects(rebuildArtifactRelease({ ...fixture, runNpm: () => assert.fail("not a CPU-only installation") }),
      /unexpectedly installed CUDA plugins/);
  });
}

test("CUDA expectations reject contradictory policies before npm runs", async (t) => {
  const fixture = await artifactFixture(t, true);
  for (const options of [{ expectCpu: true }, { accelerator: "cpu" }]) {
    await assert.rejects(installArtifactRelease({ ...fixture, ...options, expectCuda: true,
      runNpm: () => assert.fail("contradictory expectations"),
    }), /conflicts with CPU/);
  }
  const cpuOnly = await artifactFixture(t);
  await assert.rejects(installArtifactRelease({ ...cpuOnly, expectCuda: true,
    runNpm: () => assert.fail("no CUDA component contract"),
  }), /requires a platform with CUDA components/);
});

test("auto artifact smoke with GPU expectations accepts a verified complete CUDA installation", async (t) => {
  const fixture = await artifactFixture(t, true);
  await smokeArtifactRelease({ ...fixture, expectCuda: true,
    runNpm: async (_, args, { env }) => {
      assert.equal(env.ZG_INSTALL_ACCELERATOR, undefined); // Expectation must not force download policy.
      await installedArtifact(fixture, args[args.indexOf("--prefix") + 1]);
    }, probe: async () => {},
  });
  const report = JSON.parse(await readFile(join(fixture.prefixRoot, "installation.json"), "utf8"));
  assert.equal(report.expectCuda, true);
});

test("CUDA expectations also reject a schema-1 CPU-only installation", async (t) => {
  const fixture = await artifactFixture(t, true);
  await assert.rejects(smokeArtifactRelease({ ...fixture, expectCuda: true,
    runNpm: async (_, args) => installedArtifact(
      { ...fixture, entry: { ...fixture.entry, components: undefined } },
      args[args.indexOf("--prefix") + 1], defaultReleaseVariant(fixture.entry)),
    probe: () => assert.fail("CPU-only format must not bypass CUDA verification"),
  }), /required CUDA plugins missing/);
});

test("CPU auto verification rejects CUDA requests even if optional installation silently falls back", async (t) => {
  const fixture = await artifactFixture(t, true);
  await assert.rejects(installArtifactRelease({ ...fixture, expectCpu: true,
    runNpm: async (_, args, { env }) => {
      assert.equal(env.ZG_INSTALL_ACCELERATOR, undefined);
      assert.equal(env.ZG_PLUGIN_DIR, "");
      assert.equal(env.npm_config_offline, "false");
      assert.ok(!args.includes("--offline"));
      await (await fetch(`${env.npm_config_registry}/${encodeURIComponent(fixture.entry.components[0].package)}`)).text();
      // npm exits successfully after swallowing an optional plugin failure.
    }, verifyInstalled: () => assert.fail("the request itself must fail the check"),
  }), /requested CUDA plugins/);
  const evidence = JSON.parse(await readFile(join(fixture.prefixRoot, "cpu-downloads.json"), "utf8"));
  assert.equal(evidence.cudaRequests, 1);
});

test("CPU auto verification rejects any populated plugin cache even without a successful install", async (t) => {
  const fixture = await artifactFixture(t, true);
  await assert.rejects(installArtifactRelease({ ...fixture, expectCpu: true,
    runNpm: async (_, args, { env }) => {
      await mkdir(env.ZG_PLUGIN_CACHE, { recursive: true });
      await writeFile(join(env.ZG_PLUGIN_CACHE, "attempted-download"), "");
    }, verifyInstalled: () => ({ coreId: "fixture-core", components: [] }),
  }), /attempted to populate the CUDA cache/);
});

test("rebuild rejects a CPU installation from another run before invoking npm", async (t) => {
  const fixture = await optionalArtifactFixture(t, "linux-x64-gnu");
  await installArtifactRelease({ ...fixture, accelerator: "cpu" });
  await assert.rejects(rebuildArtifactRelease({ ...fixture, release: { ...fixture.release, commit: "another-commit" },
    runNpm: () => assert.fail("must not rebuild another release") }), /different artifact release/);
});

test("successful npm rebuild exit without fetching the real components is not accepted", async (t) => {
  const fixture = await optionalArtifactFixture(t, "linux-x64-gnu");
  const installed = await installArtifactRelease({ ...fixture, accelerator: "cpu" });
  await assert.rejects(rebuildArtifactRelease({ ...fixture,
    runNpm: async (_, args) => {
      assert.equal(args[0], "rebuild");
      assert.equal(args[args.indexOf("--prefix") + 1], installed.prefix);
    },
  }), /did not fetch this run's CUDA component/);
  const evidence = JSON.parse(await readFile(join(fixture.prefixRoot, "rebuild-downloads.json"), "utf8"));
  assert.equal(evidence.cudaRequests, 0);
  assert.equal(await readFile(join(installed.bundle, "bin/zg.exe"), "utf8"), "fixture-executable");
});

test("rebuild still verifies the installed core after every archive has been fetched", async (t) => {
  const fixture = await optionalArtifactFixture(t, "win32-x64-msvc");
  const installed = await installArtifactRelease({ ...fixture, accelerator: "cpu" });
  await assert.rejects(rebuildArtifactRelease({ ...fixture,
    runNpm: async (_, args, { env }) => {
      for (const component of fixture.entry.components) {
        const pkg = fixture.release.packages.find((candidate) => candidate.name === component.package);
        await (await fetch(`${env.npm_config_registry}/tarballs/${pkg.file}`)).arrayBuffer();
      }
      materialize({ metaRoot: installed.bundle, platformRoot: installedPackageRoot(installed.prefix, fixture.entry.package),
        entry: fixture.entry, expectedVersion: fixture.release.version,
        config: { mode: "cuda", directory: fixture.tarballRoot, cache: env.ZG_PLUGIN_CACHE, offline: true } });
      await writeFile(join(installed.bundle, "bin/zg.exe"), "unexpectedly replaced core");
    },
  }), /checksum mismatch/);
});

test("artifact smoke rejects a target that does not match the verification host", async (t) => {
  const fixture = await artifactFixture(t);
  await assert.rejects(smokeArtifactRelease({ ...fixture, target: "another-target",
    runNpm: () => assert.fail("must not install") }), /does not match host/);
});

test("registry verification resolves native dependencies nested under the meta package", async (t) => {
  const fixture = await artifactFixture(t, true);
  const prefix = fixture.prefixRoot;
  await installedArtifact(fixture, prefix);
  const metaRoot = installedPackageRoot(prefix, manifest.metaPackage);
  for (const name of [fixture.entry.package]) {
    const destination = join(metaRoot, "node_modules", name);
    await mkdir(dirname(destination), { recursive: true });
    await rename(installedPackageRoot(prefix, name), destination);
  }
  const report = await verifyInstalledRelease({ ...fixture, version: fixture.release.version, prefix });
  assert.deepEqual(report.components, fixture.entry.components.map((component) => component.package));
});

test("missing installed GPU components fail before a CPU CLI probe can mask them", async (t) => {
  const fixture = await artifactFixture(t, true);
  await assert.rejects(smokeArtifactRelease({ ...fixture,
    runNpm: async (_, args) => {
      const prefix = args[args.indexOf("--prefix") + 1];
      await installedArtifact(fixture, prefix);
      await rm(join(installedPackageRoot(prefix, manifest.metaPackage), fixture.entry.components[0].file));
    }, probe: () => assert.fail("must not accept CPU fallback"),
  }), /native payload files do not match/);
});

test("corrupt assembled libraries fail even when the executable is intact", async (t) => {
  const fixture = await artifactFixture(t, true);
  await assert.rejects(smokeArtifactRelease({ ...fixture,
    runNpm: async (_, args) => {
      const prefix = args[args.indexOf("--prefix") + 1];
      await installedArtifact(fixture, prefix);
      await writeFile(join(installedPackageRoot(prefix, manifest.metaPackage), fixture.entry.components[0].file), "corrupted");
    }, probe: () => assert.fail("must not run an incomplete installation"),
  }), /native payload checksum mismatch/);
});

test("artifact checksum failures block installation before executing any candidate", async (t) => {
  const fixture = await artifactFixture(t);
  await writeFile(join(fixture.tarballRoot, fixture.release.packages[0].file), "changed after collection");
  await assert.rejects(smokeArtifactRelease({
    ...fixture, runNpm: () => assert.fail("must not install"),
  }), /artifact tarball checksum mismatch/);
});

test("CUDA artifact smoke verifies every component but only passes meta/base to npm", async (t) => {
  const fixture = await artifactFixture(t, true);
  await smokeArtifactRelease({ ...fixture, accelerator: "cuda",
    runNpm: async (_, args) => {
      assert.equal(args.filter((arg) => arg.endsWith(".tgz") && !arg.startsWith("--")).length, 2);
      assert.ok(args.includes(`--allow-scripts=${args.at(-1)}`));
      await installedArtifact(fixture, args[args.indexOf("--prefix") + 1]);
    }, probe: async () => {},
  });
  fixture.release.packages.splice(3, 1);
  await assert.rejects(smokeArtifactRelease({ ...fixture, accelerator: "cuda",
    runNpm: () => assert.fail("must not install an incomplete set"),
  }), /artifact package identity mismatch/);
});

test("artifact identity failures reject substituted tarball paths before installation", async (t) => {
  const fixture = await artifactFixture(t);
  fixture.release.packages.at(-1).file = "../another-meta.tgz";
  await assert.rejects(smokeArtifactRelease({
    ...fixture, runNpm: () => assert.fail("must not install"),
  }), /artifact package identity mismatch/);
});

test("artifact smoke rejects the wrong native variant instead of validating a substitute package", async (t) => {
  const fixture = await artifactFixture(t);
  await assert.rejects(smokeArtifactRelease({
    ...fixture,
    runNpm: async (_, args) => installedArtifact(fixture, args[args.indexOf("--prefix") + 1],
      defaultReleaseVariant(fixture.entry) === "cuda" ? "cpu" : "cuda"),
    probe: () => assert.fail("must not run the wrong variant"),
  }), /installed artifact identity or variant mismatch/);
});

test("registry smoke installs only the meta package and exercises a distinct update and rollback", async (t) => {
  const prefixRoot = await mkdtemp(join(tmpdir(), "zg-registry-smoke-"));
  t.after(() => rm(prefixRoot, { recursive: true, force: true }));
  const events = [];
  await smokeRegistryRelease({
    manifest,
    release,
    baseline: "0.2.2",
    prefixRoot,
    registryFetch: async () => ({
      ok: true,
      json: async () => ({
        dist: { integrity: release.packages[0].integrity },
      }),
    }),
    runNpm: (_, args) => {
      assert.ok(args.includes("--registry=https://registry.npmjs.org"));
      assert.ok(!args.some((arg) => arg.endsWith(".tgz")));
      events.push(args[0] === "rebuild" ? "explicit registry plugin fetch" : args.at(-1));
    },
    verifyInstalled: async ({ version }) => {
      await mkdir(prefixRoot, { recursive: true });
      assert.equal(version, release.version);
      events.push("installed components verified");
      return { version, components: [] };
    },
    probe: async () => events.push("candidate IPC verified"),
    probePrevious: async () => events.push("baseline verified"),
  });
  assert.deepEqual(events, [
    "@zvec/zvec-grep@0.0.1",
    "installed components verified",
    "candidate IPC verified",
    ...(selectPlatform(manifest, process.platform, process.arch, process.report?.getReport().header ?? {}).components?.length
      ? ["explicit registry plugin fetch", "installed components verified"] : []),
    "@zvec/zvec-grep@0.2.2",
    "baseline verified",
    "@zvec/zvec-grep@0.0.1",
    "installed components verified",
    "candidate IPC verified",
    "@zvec/zvec-grep@0.2.2",
    "baseline verified",
  ]);
});

test("a missing registry candidate fails before any npm installation", async () => {
  await assert.rejects(
    smokeRegistryRelease({
      manifest,
      release,
      baseline: "0.2.2",
      registryFetch: async () => ({ status: 404 }),
      runNpm: () => assert.fail("must not install"),
    }),
    /registry candidate is missing/,
  );
});

test("the candidate cannot serve as its own rollback baseline", async () => {
  await assert.rejects(
    smokeRegistryRelease({ manifest, release, baseline: release.version }),
    /distinct baseline/,
  );
});

test("rollback uses current latest, or a distinct previously published version on a promotion retry", async () => {
  const registry = (metadata) => async () => ({
    ok: true,
    json: async () => metadata,
  });
  assert.equal(
    await rollbackVersion(
      manifest,
      "0.0.1",
      registry({ "dist-tags": { latest: "0.2.2" } }),
    ),
    "0.2.2",
  );
  assert.equal(
    await rollbackVersion(
      manifest,
      "0.0.1",
      registry({
        "dist-tags": { latest: "0.0.1" },
        versions: { "0.0.1": {}, "0.2.1": {}, "0.2.2": {} },
        time: { "0.2.1": "2026-09-01", "0.2.2": "2026-09-02" },
      }),
    ),
    "0.2.2",
  );
  await assert.rejects(
    rollbackVersion(
      manifest,
      "0.0.1",
      registry({ "dist-tags": { latest: "0.0.1" }, versions: { "0.0.1": {} } }),
    ),
    /distinct published baseline/,
  );
});
