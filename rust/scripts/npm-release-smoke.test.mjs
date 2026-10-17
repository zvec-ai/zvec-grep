import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { defaultReleaseVariant, loadReleaseManifest, selectPlatform, sha256File } from "./npm-package-core.mjs";
import { installedPackageRoot, rollbackVersion, smokeArtifactRelease, smokeRegistryRelease } from "./npm-release-smoke.mjs";

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

async function artifactFixture(t) {
  const root = await mkdtemp(join(tmpdir(), "zg-artifact-smoke-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const entry = selectPlatform(manifest, process.platform, process.arch,
    process.report?.getReport().header ?? {});
  const candidate = { schemaVersion: 1, version: "0.0.1", packages: [] };
  for (const [name, target] of [[entry.package, entry.target], [manifest.metaPackage, "meta"]]) {
    const file = `${name.replace(/^@/, "").replaceAll("/", "-")}-${candidate.version}.tgz`;
    await writeFile(join(root, file), `candidate artifact for ${name}`);
    candidate.packages.push({ name, target, version: candidate.version, file,
      sha256: await sha256File(join(root, file)) });
  }
  return { manifest, release: candidate, tarballRoot: root, prefixRoot: join(root, "install"), entry };
}

async function installedArtifact(fixture, prefix, variant = defaultReleaseVariant(fixture.entry)) {
  const directory = installedPackageRoot(prefix, fixture.entry.package);
  await mkdir(directory, { recursive: true });
  await writeFile(join(directory, "package.json"), JSON.stringify({
    name: fixture.entry.package, version: fixture.release.version, zvecGrep: { variant },
  }));
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
      assert.deepEqual(args.slice(-2), fixture.release.packages.map((pkg) => join(fixture.tarballRoot, pkg.file)));
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
});

test("artifact checksum failures block installation before executing any candidate", async (t) => {
  const fixture = await artifactFixture(t);
  await writeFile(join(fixture.tarballRoot, fixture.release.packages[0].file), "changed after collection");
  await assert.rejects(smokeArtifactRelease({
    ...fixture, runNpm: () => assert.fail("must not install"),
  }), /artifact tarball checksum mismatch/);
});

test("artifact identity failures reject substituted tarball paths before installation", async (t) => {
  const fixture = await artifactFixture(t);
  fixture.release.packages[1].file = "../another-meta.tgz";
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
      events.push(args.at(-1));
    },
    probe: async () => events.push("candidate IPC verified"),
    probePrevious: async () => events.push("baseline verified"),
  });
  assert.deepEqual(events, [
    "@zvec/zvec-grep@0.0.1",
    "candidate IPC verified",
    "@zvec/zvec-grep@0.2.2",
    "baseline verified",
    "@zvec/zvec-grep@0.0.1",
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
