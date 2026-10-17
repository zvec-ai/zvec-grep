import assert from "node:assert/strict";
import test from "node:test";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { loadReleaseManifest, run, sha256File } from "./npm-package-core.mjs";
import {
  checkPublishedPackage,
  collectRelease,
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
    ),
    { package: manifest.metaPackage, target: "meta" },
  ]) {
    const directory = join(root, platform.target, "package");
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
        : { os: [platform.os], cpu: [platform.cpu] }),
    };
    mutateMetadata?.(platform.target, metadata);
    await writeFile(join(directory, "package.json"), JSON.stringify(metadata));
    if (platform.target !== "meta") {
      const checksums = { target: platform.target };
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
    release.packages.map((pkg) => pkg.target),
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
    assert.equal(release.packages.length, fixture.manifest.platforms.length + 1);
  }
  await rm(join(fixture.tarballRoot, fixture.packages[0].file));
  await assert.rejects(
    collectRelease({ ...fixture, version: entry.version }),
    /do not cover exactly/,
  );
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
