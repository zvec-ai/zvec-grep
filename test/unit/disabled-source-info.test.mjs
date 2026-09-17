import assert from "node:assert/strict";
import { mkdir, readFile, readdir, realpath, stat } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { writeWorkspaceManifest } from "../../dist/engine/manifest.js";
import { createZvecGrep } from "../../dist/engine/service/zvec-grep.js";
import { acquireReadWriteLock } from "../../dist/engine/utils/lock.js";
import {
  createTemporaryDirectory,
  removeTemporaryDirectory,
} from "../helpers/fixtures.mjs";

async function fixture(t, indexPolicy = "disabled") {
  const temporary = await createTemporaryDirectory(t, "zvec-disabled-info-", {
    cleanup: false,
  });
  const root = await realpath(temporary);
  const home = join(root, ".zvec-grep");
  writeWorkspaceManifest(home, {
    manifestVersion: 1,
    id: "disabled-source-info-fixture",
    name: "disabled-source-info-fixture",
    path: home,
    rootPaths: [{ absolutePath: root, recursive: true }],
    indexPolicy,
    embedding: null,
    indexVersion: null,
    createdTime: 1,
    updatedTime: 1,
    embeddingRuntime: {},
  });
  const service = await createZvecGrep({ root });
  t.after(async () => {
    try {
      await service.close();
    } finally {
      await removeTemporaryDirectory(temporary);
    }
  });
  const manifestPath = join(home, "manifest.json");
  const manifest = await readFile(manifestPath);
  const before = await stat(manifestPath);
  return {
    root,
    home,
    service,
    async assertManifestUnchanged() {
      assert.deepEqual(await readFile(manifestPath), manifest);
      const after = await stat(manifestPath);
      for (const field of ["ino", "size", "mode", "mtimeMs", "ctimeMs"])
        assert.equal(after[field], before[field], `manifest ${field} changed`);
    },
  };
}

test("disabled source info without status creates no lock or storage and leaves its manifest unchanged", async (t) => {
  const rig = await fixture(t);
  const child = join(rig.root, "child");
  await mkdir(child);
  for (const requestedRoot of [rig.root, child]) {
    const info = await rig.service.info({
      root: requestedRoot,
      includeStatus: false,
    });
    assert.equal(info.root, rig.root);
    assert.equal(info.indexed, false);
    assert.equal(info.indexPolicy, "disabled");
    assert.equal(info.source, "unindexed");
    assert.equal(info.status, null);
    assert.equal(info.workspaceIndex.embedding, null);
    assert.deepEqual(await readdir(rig.home), ["manifest.json"]);
    await assert.rejects(stat(join(rig.home, "locks")), { code: "ENOENT" });
    await rig.assertManifestUnchanged();
  }
});

for (const nested of [false, true]) {
  test(`disabled source info still rejects an active ${nested ? "ancestor" : "workspace"} writer`, async (t) => {
    const rig = await fixture(t);
    const requestedRoot = nested ? join(rig.root, "child") : rig.root;
    if (nested) await mkdir(requestedRoot);
    const lock = acquireReadWriteLock(
      join(rig.home, "locks", "home"),
      "write",
      { operation: "fixture-active-index-writer" },
    );
    const ownerPath = join(lock.path, "lock.json");
    const owner = await readFile(ownerPath);
    try {
      await assert.rejects(
        rig.service.info({ root: requestedRoot, includeStatus: false }),
        (error) => {
          assert.equal(error.code, "ZVEC_GREP.ENGINE.LOCK.BUSY");
          assert.match(error.context, /fixture-active-index-writer/);
          return true;
        },
      );
      assert.deepEqual(
        await readFile(ownerPath),
        owner,
        "the active writer must not be removed or modified",
      );
      assert.deepEqual(await readdir(join(rig.home, "locks")), ["home.write"]);
      await rig.assertManifestUnchanged();
    } finally {
      lock.release();
    }
  });
}

test("disabled source info with status retains the existing read-lock behavior", async (t) => {
  const rig = await fixture(t);
  const info = await rig.service.info({ includeStatus: true });
  assert.equal(info.indexed, false);
  assert.equal(info.status, null);
  assert.deepEqual(await readdir(join(rig.home, "locks")), ["home.readers"]);
  assert.deepEqual(await readdir(join(rig.home, "locks", "home.readers")), []);
  await rig.assertManifestUnchanged();
});

test("enabled metadata-only info retains its read lock rather than broadening the disabled exception", async (t) => {
  const rig = await fixture(t, "enabled");
  const info = await rig.service.info({ includeStatus: false });
  assert.equal(info.indexPolicy, "enabled");
  assert.equal(info.indexed, false);
  assert.equal(info.status, null);
  assert.deepEqual(await readdir(join(rig.home, "locks")), ["home.readers"]);
  assert.deepEqual(await readdir(join(rig.home, "locks", "home.readers")), []);
  await rig.assertManifestUnchanged();
});
