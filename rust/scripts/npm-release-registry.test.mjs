import assert from "node:assert/strict";
import test from "node:test";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, readdir, rm, truncate, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { cargoPackageVersion, loadReleaseManifest, MAX_NPM_TARBALL_BYTES, nativePackageEntries, run, sha256File } from "./npm-package-core.mjs";
import { coreIdentity } from "../npm/meta/plugins.cjs";
import { materialize, verifyInstallation } from "../npm/meta/install.cjs";
import { smokeRegistryRelease } from "./npm-release-smoke.mjs";
import {
  checkPublishedPackage,
  collectRelease,
  stageVerificationMetadata,
  promoteRelease,
  publishRelease,
} from "./npm-release-registry.mjs";

const entry = {
  name: "@zvec/zvec-grep",
  version: "0.0.1",
  integrity: "sha512-verified-tarball",
};

test("allows an unpublished version and an identical partial-release retry", async () => {
  assert.equal(
    await checkPublishedPackage(entry, async () => ({ status: 404 })),
    false,
  );
  assert.equal(
    await checkPublishedPackage(entry, async () => ({
      ok: true,
      json: async () => ({ dist: { integrity: entry.integrity } }),
    })),
    true,
  );
});

test("rejects a version collision before npm publishing", async () => {
  await assert.rejects(
    checkPublishedPackage(entry, async () => ({
      ok: true,
      json: async () => ({ dist: { integrity: "sha512-another-tarball" } }),
    })),
    /already contains different bytes/,
  );
});

test("does not treat a registry outage as an unpublished version", async () => {
  await assert.rejects(
    checkPublishedPackage(entry, async () => ({ status: 503, ok: false })),
    /registry returned 503/,
  );
});

async function releaseFixture(
  t,
  { missingCompliance, invalidSbom, targets, mutateMetadata, mutateChecksums } = {},
) {
  const root = await mkdtemp(join(tmpdir(), "zg-release-registry-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const manifest = loadReleaseManifest();
  const packages = [];
  for (const platform of [
    ...manifest.platforms.filter(
      (platform) => targets === undefined || targets.includes(platform.target),
    ).flatMap((platform) => nativePackageEntries(platform).map((pkg) => ({
      ...platform, package: pkg.name, component: pkg.component,
    }))),
    { package: manifest.metaPackage, target: "meta" },
  ]) {
    const directory = join(root, platform.package.replaceAll("/", "-"), "package");
    await mkdir(directory, { recursive: true });
    const metadata = {
      name: platform.package,
      version: entry.version,
      ...(platform.target === "meta"
        ? {
            optionalDependencies: Object.fromEntries(
              manifest.platforms.map((candidate) => [
                candidate.package,
                entry.version,
              ]),
            ),
          }
        : { os: [platform.os], cpu: [platform.cpu], ...(platform.libc ? { libc: [platform.libc] } : {}) }),
    };
    mutateMetadata?.(platform.target, metadata);
    await writeFile(join(directory, "package.json"), JSON.stringify(metadata));
    if (platform.target !== "meta") {
      const component = platform.components?.find((candidate) => candidate.package === platform.component);
      const empty = { bytes: 0, sha256: createHash("sha256").update("").digest("hex") };
      const checksums = { schemaVersion: !component && platform.components?.length ? 3 : 1,
        target: platform.target,
        ...(component ? { component: component.package, files: { [component.payload]: empty } }
          : { files: { [`bin/${platform.binary}`]: empty } }),
      };
      if (!component && platform.components?.length) {
        checksums.coreId = coreIdentity(platform.target, entry.version, checksums.files);
        const plugins = { schemaVersion: 1, target: platform.target, version: entry.version,
          coreId: checksums.coreId, files: Object.fromEntries(platform.components.map((c) => [c.file, empty])),
          components: platform.components.map((component) => {
            const pkg = packages.find((pkg) => pkg.name === component.package);
            return { ...component, version: entry.version, checksum: empty,
              tarball: pkg.file, archiveBytes: pkg.bytes, integrity: pkg.integrity };
          }) };
        const raw = JSON.stringify(plugins);
        checksums.pluginsSha256 = createHash("sha256").update(raw).digest("hex");
        await writeFile(join(directory, "plugins.json"), raw);
      }
      mutateChecksums?.(platform.target, checksums);
      await writeFile(
        join(directory, "checksums.json"),
        JSON.stringify(checksums),
      );
    }
    if (platform.target !== "meta" && platform.target !== missingCompliance) {
      await mkdir(join(directory, "LICENSES"));
      await writeFile(
        join(directory, "LICENSES", "example.txt"),
        "Example license text",
      );
      await writeFile(
        join(directory, "SBOM.spdx.json"),
        JSON.stringify(
          invalidSbom
            ? {}
            : {
                spdxVersion: "SPDX-2.3",
                dataLicense: "CC0-1.0",
                SPDXID: "SPDXRef-DOCUMENT",
                documentNamespace: `https://example.com/${platform.target}/0.0.1`,
                creationInfo: {
                  creators: ["Organization: Example"],
                  created: "2026-10-13T00:00:00Z",
                },
                packages: [
                  { name: platform.package, SPDXID: "SPDXRef-Package" },
                ],
              },
        ),
      );
    }
    const file = `${platform.package.replace(/^@/, "").replaceAll("/", "-")}-${entry.version}.tgz`;
    const path = join(root, file);
    run("tar", ["-czf", path, "-C", join(directory, ".."), "package"]);
    packages.push({
      name: platform.package,
      target: platform.target,
      ...(platform.component ? { component: platform.component } : {}),
      version: entry.version,
      file,
      bytes: (await readFile(path)).length,
      sha256: await sha256File(path),
      integrity: `sha512-${createHash("sha512")
        .update(await readFile(path))
        .digest("base64")}`,
    });
  }
  await writeFile(
    join(root, "release.json"),
    JSON.stringify({ schemaVersion: 1, version: entry.version, packages }),
  );
  return { manifest, tarballRoot: root, packages };
}

test("collects selected platforms in manifest order as a validation-only candidate", async (t) => {
  const targets = ["win32-x64-msvc", "linux-x64-gnu"];
  const fixture = await releaseFixture(t, { targets });
  const summaryPath = join(fixture.tarballRoot, "summary.txt");
  const previousSummary = process.env.GITHUB_STEP_SUMMARY;
  process.env.GITHUB_STEP_SUMMARY = summaryPath;
  t.after(() => {
    if (previousSummary === undefined) delete process.env.GITHUB_STEP_SUMMARY;
    else process.env.GITHUB_STEP_SUMMARY = previousSummary;
  });
  const release = await collectRelease({
    ...fixture,
    version: entry.version,
    targets,
  });
  assert.equal(release.partial, true);
  assert.deepEqual(release.targets, ["linux-x64-gnu", "win32-x64-msvc"]);
  assert.deepEqual(
    release.packages.filter((pkg) => !pkg.component).map((pkg) => pkg.target),
    [...release.targets, "meta"],
  );
  assert.deepEqual(
    JSON.parse(await readFile(join(fixture.tarballRoot, "release.json"), "utf8")),
    release,
  );
  assert.equal(
    await readFile(join(fixture.tarballRoot, "SHA256SUMS"), "utf8"),
    release.packages.map((pkg) => `${pkg.sha256}  ${pkg.file}\n`).join(""),
  );
  const instructions = await readFile(
    join(fixture.tarballRoot, "README.md"),
    "utf8",
  );
  assert.match(
    instructions,
    /Validation only.*partial candidate, not a complete release/,
  );
  assert.match(instructions, /# linux-x64-gnu\nnpm install/);
  assert.match(instructions, /# win32-x64-msvc\nnpm install/);
  assert.doesNotMatch(instructions, /# darwin-/);
  const summary = await readFile(summaryPath, "utf8");
  assert.match(summary, /rust-candidate-0\.0\.1/);
  assert.match(summary, /validation-only/);
  assert.doesNotMatch(summary, /rust-release-/);
  const meta = JSON.parse(
    run(
      "tar",
      [
        "-xOf",
        join(fixture.tarballRoot, release.packages.at(-1).file),
        "package/package.json",
      ],
      { capture: true },
    ),
  );
  assert.deepEqual(
    meta.optionalDependencies,
    Object.fromEntries(
      fixture.manifest.platforms.map((platform) => [platform.package, entry.version]),
    ),
  );
});

test("default collection still requires every platform and treats all targets as a full release", async (t) => {
  const fixture = await releaseFixture(t);
  const allTargets = fixture.manifest.platforms.map((platform) => platform.target);
  for (const targets of [undefined, [...allTargets].reverse()]) {
    const release = await collectRelease({
      ...fixture,
      version: entry.version,
      targets,
    });
    assert.equal(release.partial, false);
    assert.deepEqual(release.targets, allTargets);
    assert.equal(release.packages.length, fixture.manifest.platforms.flatMap(nativePackageEntries).length + 1);
  }
  await rm(join(fixture.tarballRoot, fixture.packages[0].file));
  await assert.rejects(
    collectRelease({ ...fixture, version: entry.version }),
    /do not cover exactly/,
  );
});

test("verification metadata preserves collected bytes and supports registry smoke without native tarballs", async (t) => {
  const fixture = await releaseFixture(t);
  const release = await collectRelease({ ...fixture, version: entry.version });
  const destination = join(fixture.tarballRoot, "verification");
  await stageVerificationMetadata({ ...fixture, release, destination });
  const meta = release.packages.find((pkg) => pkg.target === "meta");
  assert.deepEqual((await readdir(destination)).sort(), [meta.file, "release.json", "SHA256SUMS", "README.md"].sort());
  assert.equal(await sha256File(join(destination, meta.file)), meta.sha256);
  assert.equal(await readFile(join(destination, "release.json"), "utf8"), await readFile(join(fixture.tarballRoot, "release.json"), "utf8"));
  for (const pkg of release.packages) assert.equal(await sha256File(join(fixture.tarballRoot, pkg.file)), pkg.sha256);
  for (const pkg of release.packages.filter((pkg) => pkg.target !== "meta")) await rm(join(fixture.tarballRoot, pkg.file));
  const prefixRoot = join(destination, "registry-smoke");
  let verifiedPackages = 0;
  let probes = 0;
  await smokeRegistryRelease({
    manifest: fixture.manifest,
    release: JSON.parse(await readFile(join(destination, "release.json"), "utf8")),
    baseline: "0.2.2", prefixRoot,
    registryFetch: async (url) => {
      const pkg = release.packages.find((pkg) => url.endsWith(`/${encodeURIComponent(pkg.name)}/${pkg.version}`));
      assert.ok(pkg);
      verifiedPackages++;
      return { ok: true, json: async () => ({ dist: { integrity: pkg.integrity } }) };
    },
    runNpm: (_, args) => {
      assert.ok(args.includes("--registry=https://registry.npmjs.org"));
      assert.ok(!args.some((arg) => arg.endsWith(".tgz")));
    },
    verifyInstalled: async () => { await mkdir(prefixRoot, { recursive: true }); return { components: [] }; },
    probe: async () => { probes++; },
    probePrevious: async () => {},
  });
  assert.equal(verifiedPackages, release.packages.length);
  assert.equal(probes, 2);
  await writeFile(join(fixture.tarballRoot, meta.file), "changed after collection");
  await assert.rejects(stageVerificationMetadata({ ...fixture, release, destination }), /invalid collected meta package/);
});

test("collection rejects empty, duplicate, and unknown target selections", async (t) => {
  const fixture = await releaseFixture(t);
  for (const targets of [
    [],
    [""],
    ["unknown"],
    ["linux-x64-gnu", "linux-x64-gnu"],
    "linux-x64-gnu",
  ]) {
    await assert.rejects(
      collectRelease({ ...fixture, version: entry.version, targets }),
      /nonempty list of unique known platforms/,
    );
  }
});

test("partial collection rejects missing and extra tarballs, including a missing meta package", async (t) => {
  const targets = ["linux-x64-gnu"];
  for (const missingTarget of ["linux-x64-gnu", "meta"]) {
    const fixture = await releaseFixture(t, { targets });
    await rm(
      join(
        fixture.tarballRoot,
        fixture.packages.find((pkg) => pkg.target === missingTarget).file,
      ),
    );
    await assert.rejects(
      collectRelease({ ...fixture, version: entry.version, targets }),
      /do not cover exactly/,
    );
  }
  const extra = await releaseFixture(t);
  await assert.rejects(
    collectRelease({ ...extra, version: entry.version, targets }),
    /do not cover exactly/,
  );
});

test("partial collection validates native and meta identities and platform metadata", async (t) => {
  const targets = ["linux-x64-gnu"];
  for (const target of [...targets, "meta"]) {
    for (const field of ["name", "version"]) {
      const fixture = await releaseFixture(t, {
        targets,
        mutateMetadata: (candidate, metadata) => {
          if (candidate === target) metadata[field] = "wrong";
        },
      });
      await assert.rejects(
        collectRelease({ ...fixture, version: entry.version, targets }),
        /package identity mismatch/,
      );
    }
  }
  for (const field of ["os", "cpu"]) {
    const fixture = await releaseFixture(t, {
      targets,
      mutateMetadata: (target, metadata) => {
        if (target !== "meta") metadata[field] = ["wrong"];
      },
    });
    await assert.rejects(
      collectRelease({ ...fixture, version: entry.version, targets }),
      /package platform mismatch/,
    );
  }
  const invalidTarget = await releaseFixture(t, {
    targets,
    mutateChecksums: (_, checksums) => {
      checksums.target = "wrong";
    },
  });
  await assert.rejects(
    collectRelease({ ...invalidTarget, version: entry.version, targets }),
    /package platform mismatch/,
  );
  const incompleteMeta = await releaseFixture(t, {
    targets,
    mutateMetadata: (target, metadata) => {
      if (target === "meta") {
        delete metadata.optionalDependencies[loadReleaseManifest().platforms[0].package];
      }
    },
  });
  await assert.rejects(
    collectRelease({ ...incompleteMeta, version: entry.version, targets }),
    /does not lock every platform/,
  );
});

test("partial candidates are rejected before registry access or npm commands", async (t) => {
  for (const targets of [["linux-x64-gnu"], undefined]) {
    const fixture = await releaseFixture(t, { targets });
    const release = await collectRelease({
      ...fixture,
      version: entry.version,
      targets,
    });
    // Even a complete package list cannot bypass an explicit partial marker.
    release.partial = true;
    await writeFile(
      join(fixture.tarballRoot, "release.json"),
      JSON.stringify(release),
    );
    for (const action of [publishRelease, promoteRelease]) {
      await assert.rejects(
        action({
          ...fixture,
          registryFetch: () => assert.fail("must not access the registry"),
          runNpm: () => assert.fail("must not run npm"),
        }),
        /partial validation candidates cannot be published or promoted/,
      );
    }
  }
});

test("the targets CLI option is only accepted for collect", () => {
  const result = spawnSync(
    process.execPath,
    [
      fileURLToPath(new URL("./npm-release.mjs", import.meta.url)),
      "verify",
      "--targets",
      "linux-x64-gnu,win32-x64-msvc",
    ],
    { encoding: "utf8" },
  );
  assert.equal(result.status, 1);
  assert.match(result.stderr, /--targets is only supported by collect/);
});

function publishedRegistry(packages) {
  return async (url) => {
    const candidate = packages.find((pkg) =>
      url.includes(encodeURIComponent(pkg.name)),
    );
    return {
      ok: true,
      json: async () => ({ dist: { integrity: candidate.integrity } }),
    };
  };
}

test("publishes new packages only to rust-canary and publishes the meta package last", async (t) => {
  const fixture = await releaseFixture(t);
  const commands = [];
  await publishRelease({
    ...fixture,
    registryFetch: async () => ({ status: 404 }),
    runNpm: (_, args) => commands.push(args),
  });
  assert.equal(commands.length, fixture.packages.length);
  assert.ok(
    commands.every(
      (args) => args[0] === "publish" && args.at(-1) === "rust-canary",
    ),
  );
  assert.ok(commands.at(-1)[1].endsWith(fixture.packages.at(-1).file));
});

test("identical candidate retries can only change rust-canary, never latest", async (t) => {
  const fixture = await releaseFixture(t);
  const commands = [];
  await publishRelease({
    ...fixture,
    registryFetch: publishedRegistry(fixture.packages),
    runNpm: (_, args) => commands.push(args),
  });
  assert.ok(
    commands.every(
      (args) => args[0] === "dist-tag" && args.at(-1) === "rust-canary",
    ),
  );
});

test("missing compliance in any native tarball blocks every npm mutation", async (t) => {
  const fixture = await releaseFixture(t, {
    missingCompliance: loadReleaseManifest().platforms.at(-1).target,
  });
  const commands = [];
  await assert.rejects(
    publishRelease({
      ...fixture,
      registryFetch: publishedRegistry(fixture.packages),
      runNpm: (_, args) => commands.push(args),
    }),
    /publication disabled.*LICENSES and SBOM/,
  );
  assert.deepEqual(commands, []);
});

test("an invalid SPDX document also blocks publishing", async (t) => {
  const fixture = await releaseFixture(t, { invalidSbom: true });
  await assert.rejects(
    publishRelease({
      ...fixture,
      registryFetch: async () => ({ status: 404 }),
      runNpm: () => assert.fail("must not publish"),
    }),
    /invalid SPDX SBOM/,
  );
});

test("promotion only retags already published identical packages with the meta tag last", async (t) => {
  const fixture = await releaseFixture(t);
  const commands = [];
  await promoteRelease({
    ...fixture,
    registryFetch: publishedRegistry(fixture.packages),
    runNpm: (_, args) => commands.push(args),
  });
  assert.deepEqual(
    commands,
    fixture.packages.map((pkg) => [
      "dist-tag",
      "add",
      `${pkg.name}@${pkg.version}`,
      "latest",
    ]),
  );
});

test("an incomplete registry release cannot be promoted or published by the promotion command", async (t) => {
  const fixture = await releaseFixture(t);
  await assert.rejects(
    promoteRelease({
      ...fixture,
      registryFetch: async () => ({ status: 404 }),
      runNpm: () => assert.fail("must not retag or publish"),
    }),
    /incomplete registry release/,
  );
});

test("oversized candidates fail collection and publication before any npm mutation", async (t) => {
  const fixture = await releaseFixture(t);
  // Inflate the final tarball as a sparse file: no hundreds-of-MiB fixture data.
  await truncate(join(fixture.tarballRoot, fixture.packages.at(-1).file), MAX_NPM_TARBALL_BYTES + 1);
  await assert.rejects(collectRelease({ ...fixture, version: entry.version }), /exceeds 350 MiB/);
  for (const action of [publishRelease, promoteRelease]) {
    await assert.rejects(action({ ...fixture,
      registryFetch: publishedRegistry(fixture.packages),
      runNpm: () => assert.fail("must not mutate npm"),
    }), /exceeds 350 MiB/);
  }
});

test("collection requires every referenced component and rejects mandatory CUDA dependencies", async (t) => {
  const targets = ["linux-x64-gnu"];
  const missing = await releaseFixture(t, { targets });
  const component = missing.packages.find((pkg) => pkg.component?.endsWith("ort-cuda-part2"));
  await rm(join(missing.tarballRoot, component.file));
  await assert.rejects(collectRelease({ ...missing, version: entry.version, targets }), /do not cover exactly/);
  const wrongVersion = await releaseFixture(t, { targets, mutateMetadata: (_, metadata) => {
    if (metadata.name === "@zvec/zvec-grep-linux-x64-gnu") metadata.dependencies = { "@zvec/zvec-grep-linux-x64-gnu-ggml-cuda13": "0.0.2" };
  } });
  await assert.rejects(collectRelease({ ...wrongVersion, version: entry.version, targets }), /does not lock every runtime component/);
});

test("components publish before the base package that depends on them", async (t) => {
  const fixture = await releaseFixture(t);
  const commands = [];
  await publishRelease({ ...fixture, registryFetch: async () => ({ status: 404 }), runNpm: (_, args) => commands.push(args) });
  const published = commands.map((args) => args[1]);
  for (const platform of fixture.manifest.platforms) {
    const baseIndex = fixture.packages.findIndex((pkg) => pkg.name === platform.package);
    for (const component of platform.components ?? []) {
      const index = fixture.packages.findIndex((pkg) => pkg.name === component.package);
      assert.ok(index < baseIndex);
      assert.ok(published[index].endsWith(fixture.packages[index].file));
    }
  }
});

for (const target of ["linux-x64-gnu", "win32-x64-msvc"]) {
  test(`manual ${target} packaging rejects mixed Cargo output through every CPU entry point`, async (t) => {
    const root = await mkdtemp(join(tmpdir(), "zg-cpu-pack-"));
    t.after(() => rm(root, { recursive: true, force: true }));
    const platform = loadReleaseManifest().platforms.find((p) => p.target === target);
    const cargoTarget = join(root, "target");
    const profile = join(cargoTarget, "release");
    const windows = platform.os === "win32";
    const cpuFiles = windows ? ["zvec_c_api.dll", "onnxruntime.dll", "DirectML.dll"]
      : ["libzvec_c_api.so", "libonnxruntime.so.1.27.0"];
    const staleFiles = windows ? ["ggml-cuda.dll", "onnxruntime_providers_cuda.dll", "llama.dll"]
      : ["libggml-cuda.so", "libonnxruntime_providers_cuda.so", "libllama.so.0"];
    await mkdir(profile, { recursive: true });
    const binary = join(profile, platform.binary);
    await writeFile(binary, "old CUDA executable fixture, never executed");
    for (const file of staleFiles) await writeFile(join(profile, file), `old CUDA ${file}`);
    // CPU rebuild overwrites zg/ORT but leaves old CUDA libraries in the profile.
    await writeFile(binary, "new CPU executable fixture, never executed");
    for (const file of cpuFiles) await writeFile(join(profile, file), `current CPU ${file}`);
    const separateBinary = join(root, platform.binary);
    await writeFile(separateBinary, "new CPU executable fixture, never executed");
    const output = join(root, "release");
    const script = fileURLToPath(new URL("./npm-release.mjs", import.meta.url));
    const args = [script, "pack-platform", "--target", target, "--no-build", "--output-dir", output];
    const env = { ...process.env, CARGO_TARGET_DIR: cargoTarget };
    for (const extra of [[], ["--binary", binary], ["--binary", separateBinary, "--lib-dir", profile]]) {
      const result = spawnSync(process.execPath, [...args, ...extra], { env, encoding: "utf8", timeout: 60_000 });
      assert.ifError(result.error);
      assert.equal(result.status, 1, result.stdout + result.stderr);
      assert.match(result.stderr, /refusing CPU-only packaging/);
      await assert.rejects(readdir(join(output, "tarballs")), { code: "ENOENT" });
      for (const file of staleFiles) assert.equal(await readFile(join(profile, file), "utf8"), `old CUDA ${file}`);
      // An explicit --binary/--lib-dir must not borrow the CUDA record's trust.
      await writeFile(join(cargoTarget, "native-build.json"), JSON.stringify({
        target, variant: "cuda", profileDir: profile, bundleDir: join(profile, "zg-cuda"),
      }));
    }
    for (const file of staleFiles) await rm(join(profile, file));
    run(process.execPath, [...args, "--binary", separateBinary, "--lib-dir", profile], { capture: true, env });
    const staged = join(output, "staging", target);
    const metadata = JSON.parse(await readFile(join(staged, "package.json"), "utf8"));
    assert.equal(metadata.zvecGrep.variant, "cpu");
    const checksums = JSON.parse(await readFile(join(staged, "checksums.json"), "utf8"));
    const expected = [`bin/${platform.binary}`, ...cpuFiles.map((file) => `lib/${file}`)].sort();
    assert.deepEqual(Object.keys(checksums.files).sort(), expected);
    assert.deepEqual((await readdir(join(staged, "lib"))).sort(), [...cpuFiles].sort());
    const tarballs = await readdir(join(output, "tarballs"));
    assert.equal(tarballs.length, 1);
    const packedFiles = run("tar", ["-tzf", join(output, "tarballs", tarballs[0])], { capture: true })
      .trim().split(/\r?\n/).filter((file) => /^package\/(bin|lib)\/.+/.test(file)).sort();
    assert.deepEqual(packedFiles, expected.map((file) => `package/${file}`).sort());
  });

  test(`actual npm packing pins plugin bytes and collects an installable ${target} candidate`, async (t) => {
    const root = await mkdtemp(join(tmpdir(), "zg-plugin-pack-"));
    t.after(() => rm(root, { recursive: true, force: true }));
    const platform = loadReleaseManifest().platforms.find((p) => p.target === target);
    const bundle = join(root, "bundle");
    const files = [`bin/${platform.binary}`, `lib/${platform.os === "win32" ? "zvec_c_api.dll" : "libzvec_c_api.so"}`,
      ...new Set(platform.components.map((c) => c.file))];
    for (const file of files) {
      await mkdir(join(bundle, file, ".."), { recursive: true });
      await writeFile(join(bundle, file), `fixture only, never executed: ${file}\n`);
    }
    const output = join(root, "release");
    const script = fileURLToPath(new URL("./npm-release.mjs", import.meta.url));
    run(process.execPath, [script, "pack-platform", "--target", target, "--no-build",
      "--bundle-dir", bundle, "--output-dir", output], { capture: true });
    run(process.execPath, [script, "collect", "--targets", target, "--output-dir", output], { capture: true });
    const release = JSON.parse(await readFile(join(output, "tarballs/release.json"), "utf8"));
    assert.equal(release.partial, true);
    assert.equal(release.packages.length, platform.components.length + 2);
    const platformRoot = join(output, "staging", target);
    const metadata = JSON.parse(await readFile(join(platformRoot, "package.json"), "utf8"));
    assert.equal(metadata.dependencies, undefined);
    assert.deepEqual(metadata.zvecGrep.bundledAccelerators, ["cpu"]);
    const options = { entry: platform, expectedVersion: cargoPackageVersion(), platformRoot, metaRoot: join(root, "installed"),
      config: { mode: "cpu", directory: join(output, "tarballs"), cache: join(root, "cache"), offline: true } };
    materialize(options);
    assert.deepEqual(verifyInstallation(options).components, []);
    materialize({ ...options, config: { ...options.config, mode: "cuda" } });
    assert.equal(verifyInstallation({ ...options, requireCuda: true }).components.length, platform.components.length);
  });
}
