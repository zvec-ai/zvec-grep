import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { loadReleaseManifest } from "./npm-package-core.mjs";
import { rollbackVersion, smokeRegistryRelease } from "./npm-release-smoke.mjs";

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
