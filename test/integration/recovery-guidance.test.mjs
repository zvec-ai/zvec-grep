import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { hostname } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { acquireReadWriteLock } from "../../dist/engine/utils/lock.js";
import { assertHomeNotIncomplete } from "../../dist/engine/manifest.js";
import { reserveDestination } from "../../dist/engine/reservation.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { useIsolatedZvecGrepHome } from "../helpers/isolated-home.mjs";
useIsolatedZvecGrepHome();
const exec = promisify(execFile);
for (const mode of ["read", "write"])
  test(`F8 active ${mode} lock says wait and preserves its owner`, async (t) => {
    const home = await createTemporaryDirectory(t, "zg-recovery-hint-");
    const path = join(home, "locks", "home");
    const lock = acquireReadWriteLock(path, mode, {
      operation: "active-control",
    });
    t.after(() => lock.release());
    const record = join(lock.path, "lock.json"),
      before = await readFile(record, "utf8");
    assert.throws(
      () => acquireReadWriteLock(path, "write", { operation: "competitor" }),
      (error) => {
        assert.equal(error.code, "ZVEC_GREP.ENGINE.LOCK.BUSY");
        assert.match(error.context, /ownerState=alive/);
        assert.match(error.context, /Wait for it to finish/);
        assert.match(error.context, /Do not remove an active lock/);
        return true;
      },
    );
    assert.equal(await readFile(record, "utf8"), before);
  });
for (const state of ["dead", "unknown"])
  test(`F8 ${state} writer requires checked recovery and keeps the lock`, async (t) => {
    const home = await createTemporaryDirectory(t, "zg-recovery-hint-");
    const path = join(home, "locks", "home"),
      lock = path + ".write";
    await mkdir(lock, { recursive: true });
    const ended = await exec(process.execPath, [
      "-e",
      "console.log(process.pid)",
    ]);
    const record = join(lock, "lock.json"),
      bytes = JSON.stringify({
        token: "owned",
        pid: Number(ended.stdout.trim()),
        hostname: state === "dead" ? hostname() : "foreign-host",
        startedAt: Date.now(),
        operation: "interrupted",
      });
    await writeFile(record, bytes);
    assert.throws(
      () => acquireReadWriteLock(path, "read", { operation: "status" }),
      (error) => {
        assert.match(error.context, new RegExp(`ownerState=${state}`));
        if (state === "dead") {
          assert.match(error.context, /quarantine/);
          assert.match(
            error.context,
            /Do not remove only a lock or the INCOMPLETE marker/,
          );
        } else {
          assert.match(error.context, /cannot be confirmed/);
          assert.match(error.context, /Do not remove an unconfirmed lock/);
        }
        return true;
      },
    );
    assert.equal(await readFile(record, "utf8"), bytes);
  });
test("F8 incomplete readers and reservation errors require the same quarantine procedure", async (t) => {
  const home = await createTemporaryDirectory(t, "zg-recovery-incomplete-");
  const marker = join(home, "INCOMPLETE");
  await writeFile(marker, "preserve-marker");
  for (const run of [
    () => assertHomeNotIncomplete(home),
    () => reserveDestination({ destinationHome: home, operation: "retry" }),
  ])
    assert.throws(run, (error) => {
      assert.match(error.context, /quarantine/);
      assert.match(
        error.context,
        /Do not remove only a lock or the INCOMPLETE marker/,
      );
      return true;
    });
  assert.equal(await readFile(marker, "utf8"), "preserve-marker");
});
test("F8 CLI status reports incomplete quarantine guidance without changing the marker", async (t) => {
  const root = await createTemporaryDirectory(t, "zg-recovery-status-");
  const home = join(root, ".zvec-grep");
  await mkdir(home);
  const marker = join(home, "INCOMPLETE");
  await writeFile(marker, "preserve-marker");
  await assert.rejects(
    exec(
      process.execPath,
      [resolve("dist/cli/index.js"), "--status", root, "--mode", "direct"],
      { timeout: 30000 },
    ),
    (error) => {
      assert.equal(error.code, 1);
      assert.match(error.stderr, /quarantine/);
      assert.match(
        error.stderr,
        /Do not remove only a lock or the INCOMPLETE marker/,
      );
      return true;
    },
  );
  assert.equal(await readFile(marker, "utf8"), "preserve-marker");
});
