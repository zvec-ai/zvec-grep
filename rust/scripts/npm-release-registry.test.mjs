import assert from "node:assert/strict";
import test from "node:test";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadReleaseManifest, run, sha256File } from "./npm-package-core.mjs";
import {
  checkPublishedPackage,
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

async function releaseFixture(t, { missingCompliance, invalidSbom } = {}) {
  const root = await mkdtemp(join(tmpdir(), "zg-release-registry-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const manifest = loadReleaseManifest();
  const packages = [];
  for (const platform of [
    ...manifest.platforms,
    { package: manifest.metaPackage, target: "meta" },
  ]) {
    const directory = join(root, platform.target, "package");
    await mkdir(directory, { recursive: true });
    await writeFile(
      join(directory, "package.json"),
      JSON.stringify({ name: platform.package, version: entry.version }),
    );
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
    run("tar", ["-czf", path, "-C", join(root, platform.target), "package"]);
    packages.push({
      name: platform.package,
      target: platform.target,
      version: entry.version,
      file,
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
