import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import mutableFs, {
  mkdir,
  readFile,
  readdir,
  stat,
  writeFile,
} from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { hostname } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { acquireModelArtifactCacheLock } from "../../../dist/engine/models/artifact-cache-lock.js";
import { createTemporaryDirectory } from "../../helpers/fixtures.mjs";

function options(overrides = {}) {
  return {
    pollMs: 5,
    staleMs: 10 * 60 * 1_000,
    heartbeatMs: 1_000,
    dependencies: { now: Date.now, setTimeout },
    ...overrides,
  };
}

function mockFsMethod(t, method, implementation) {
  const original = mutableFs[method];
  t.mock.method(mutableFs, method, (...args) =>
    implementation(original, ...args),
  );
  syncBuiltinESMExports();
  t.after(() => {
    t.mock.restoreAll();
    syncBuiltinESMExports();
  });
}

async function assertWaitsForOwner(lockPath, overrides = {}) {
  const before = await readdir(lockPath);
  const waited = new Error("waiting for the existing owner");
  await assert.rejects(
    acquireModelArtifactCacheLock(
      lockPath,
      options({
        ...overrides,
        dependencies: {
          now: Date.now,
          ...overrides.dependencies,
          setTimeout() {
            throw waited;
          },
        },
      }),
    ),
    (error) => error === waited,
  );
  assert.deepEqual(await readdir(lockPath), before);
}

async function writeOwner(lockPath, value, token = "existing-owner") {
  await mkdir(lockPath);
  const path = join(lockPath, `.owner-${token}`);
  await writeFile(
    path,
    typeof value === "string" ? value : JSON.stringify(value),
  );
  return path;
}

test("a paused initializer cannot join its successor's lock", async (t) => {
  for (const phase of ["before owner write", "after owner write"]) {
    await t.test(phase, async (t) => {
      const root = await createTemporaryDirectory(
        t,
        "zvec-artifact-lock-init-",
      );
      const lockPath = join(root, "snapshot.lock");
      const paused = Promise.withResolvers();
      const resume = Promise.withResolvers();
      let firstWrite = true;
      mockFsMethod(t, "writeFile", async (originalWriteFile, ...args) => {
        if (!firstWrite) {
          return await originalWriteFile(...args);
        }
        firstWrite = false;
        if (phase === "after owner write") {
          await originalWriteFile(...args);
        }
        paused.resolve();
        await resume.promise;
        if (phase === "before owner write") {
          await originalWriteFile(...args);
        }
      });
      let now = Date.now();
      const waited = new Error("the initializer must wait for its successor");
      let firstLock;
      let successor;
      const first = acquireModelArtifactCacheLock(
        lockPath,
        options({
          dependencies: {
            now: () => now,
            setTimeout() {
              throw waited;
            },
          },
        }),
      ).then(
        (lock) => {
          firstLock = lock;
          return undefined;
        },
        (error) => error,
      );
      try {
        await paused.promise;
        now += 10 * 60 * 1_000 + 1_000;
        successor = await acquireModelArtifactCacheLock(
          lockPath,
          options({ dependencies: { now: () => now, setTimeout } }),
        );
        await successor.assertOwned();
        const successorFiles = await readdir(lockPath);
        assert.equal(successorFiles.length, 1);

        resume.resolve();
        assert.equal(await first, waited);
        assert.deepEqual(await readdir(lockPath), successorFiles);
        await successor.assertOwned();
        assert.deepEqual(await readdir(root), ["snapshot.lock"]);
      } finally {
        resume.resolve();
        await first;
        await firstLock?.release();
        await successor?.release();
      }
      assert.deepEqual(await readdir(root), []);
    });
  }
});

test("failed owner initialization cleans up only its private files", async (t) => {
  const root = await createTemporaryDirectory(
    t,
    "zvec-artifact-lock-init-error-",
  );
  const lockPath = join(root, "snapshot.lock");
  const failure = Object.assign(new Error("owner write failed"), {
    code: "EIO",
  });
  mockFsMethod(t, "writeFile", async (originalWriteFile, ...args) => {
    await originalWriteFile(...args);
    throw failure;
  });
  await assert.rejects(
    acquireModelArtifactCacheLock(lockPath, options()),
    (error) => error === failure,
  );
  assert.deepEqual(await readdir(root), []);
});

test("refreshes the heartbeat when publishing a delayed initializer", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-lock-publish-");
  const lockPath = join(root, "snapshot.lock");
  let now = Date.now();
  mockFsMethod(t, "rename", async (originalRename, source, destination) => {
    if (destination === lockPath) {
      now += 10 * 60 * 1_000 + 1_000;
    }
    await originalRename(source, destination);
  });
  const lockOptions = options({ dependencies: { now: () => now, setTimeout } });
  const owner = await acquireModelArtifactCacheLock(lockPath, lockOptions);
  try {
    await assertWaitsForOwner(lockPath, lockOptions);
    await owner.assertOwned();
  } finally {
    await owner.release();
  }
  assert.deepEqual(await readdir(root), []);
});

test("a publisher overtaken before its first heartbeat cannot return a lock", async (t) => {
  const root = await createTemporaryDirectory(
    t,
    "zvec-artifact-lock-overtaken-",
  );
  const lockPath = join(root, "snapshot.lock");
  const paused = Promise.withResolvers();
  const resume = Promise.withResolvers();
  let firstHeartbeat = true;
  mockFsMethod(t, "utimes", async (originalUtimes, ...args) => {
    if (firstHeartbeat) {
      firstHeartbeat = false;
      paused.resolve();
      await resume.promise;
    }
    await originalUtimes(...args);
  });
  let now = Date.now();
  const waited = new Error("the displaced publisher must wait");
  let firstLock;
  let successor;
  const first = acquireModelArtifactCacheLock(
    lockPath,
    options({
      dependencies: {
        now: () => now,
        setTimeout() {
          throw waited;
        },
      },
    }),
  ).then(
    (lock) => {
      firstLock = lock;
      return undefined;
    },
    (error) => error,
  );
  try {
    await paused.promise;
    now += 10 * 60 * 1_000 + 1_000;
    successor = await acquireModelArtifactCacheLock(
      lockPath,
      options({ dependencies: { now: () => now, setTimeout } }),
    );
    const successorFiles = await readdir(lockPath);
    resume.resolve();
    assert.equal(await first, waited);
    assert.deepEqual(await readdir(lockPath), successorFiles);
    await successor.assertOwned();
    assert.deepEqual(await readdir(root), ["snapshot.lock"]);
  } finally {
    resume.resolve();
    await first;
    await firstLock?.release();
    await successor?.release();
  }
  assert.deepEqual(await readdir(root), []);
});

test("publication distinguishes directory conflicts from permission errors", async (t) => {
  const cases = [
    ["EEXIST", "directory", true],
    ["ENOTEMPTY", "directory", true],
    ["EPERM", "directory", true],
    ["EPERM", "missing", false],
    ["EPERM", "file", false],
    ["EACCES", "directory", false],
  ];
  for (const [code, destinationKind, conflict] of cases) {
    await t.test(`${code} with ${destinationKind} destination`, async (t) => {
      const root = await createTemporaryDirectory(
        t,
        "zvec-artifact-lock-conflict-",
      );
      const lockPath = join(root, "snapshot.lock");
      const failure = Object.assign(new Error("rename failed"), { code });
      const waited = new Error("waiting for the competing directory");
      mockFsMethod(
        t,
        "rename",
        async (_originalRename, _source, destination) => {
          assert.equal(destination, lockPath);
          if (destinationKind === "directory") {
            await writeOwner(lockPath, {
              token: "existing-owner",
              pid: process.pid,
              hostname: hostname(),
            });
          } else if (destinationKind === "file") {
            await writeFile(lockPath, "unrelated file");
          }
          throw failure;
        },
      );
      await assert.rejects(
        acquireModelArtifactCacheLock(
          lockPath,
          options({
            dependencies: {
              now: Date.now,
              setTimeout() {
                throw waited;
              },
            },
          }),
        ),
        (error) => error === (conflict ? waited : failure),
      );
      assert.deepEqual(
        await readdir(root),
        destinationKind === "missing" ? [] : ["snapshot.lock"],
      );
      if (destinationKind === "directory") {
        assert.deepEqual(await readdir(lockPath), [".owner-existing-owner"]);
      } else if (destinationKind === "file") {
        assert.equal(await readFile(lockPath, "utf8"), "unrelated file");
      }
    });
  }
});

test("an initial heartbeat error releases the unpublished owner's lock", async (t) => {
  const root = await createTemporaryDirectory(
    t,
    "zvec-artifact-lock-heartbeat-",
  );
  const lockPath = join(root, "snapshot.lock");
  const failure = Object.assign(new Error("heartbeat failed"), {
    code: "EACCES",
  });
  mockFsMethod(t, "utimes", async () => {
    throw failure;
  });
  await assert.rejects(
    acquireModelArtifactCacheLock(lockPath, options()),
    (error) => error === failure,
  );
  assert.deepEqual(await readdir(root), []);
});

test("recovers a killed child's fresh lock without waiting for the ten-minute TTL", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-dead-owner-");
  const lockPath = join(root, "snapshot.lock");
  const moduleUrl = new URL(
    "../../../dist/engine/models/artifact-cache-lock.js",
    import.meta.url,
  ).href;
  const child = spawn(
    process.execPath,
    [
      "--input-type=module",
      "--eval",
      `const { acquireModelArtifactCacheLock } = await import(process.argv[1]);
       await acquireModelArtifactCacheLock(process.argv[2], {
         pollMs: 5, staleMs: 600000, heartbeatMs: 1000,
         dependencies: { now: Date.now, setTimeout },
       });
       process.send("locked");
       setInterval(() => {}, 1000);`,
      moduleUrl,
      lockPath,
    ],
    { stdio: ["ignore", "ignore", "pipe", "ipc"] },
  );
  t.after(() => {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGKILL");
    }
  });
  const [message] = await once(child, "message", {
    signal: AbortSignal.timeout(5_000),
  });
  assert.equal(message, "locked");
  const [ownerName] = await readdir(lockPath);
  const owner = JSON.parse(await readFile(join(lockPath, ownerName), "utf8"));
  assert.equal(owner.pid, child.pid);
  assert.equal(owner.hostname, hostname());
  const exited = once(child, "exit");
  child.kill("SIGKILL");
  await exited;

  const replacement = await acquireModelArtifactCacheLock(
    lockPath,
    options({
      dependencies: {
        now: Date.now,
        setTimeout() {
          throw new Error(
            "an exited local owner must be reclaimed immediately",
          );
        },
      },
    }),
  );
  await replacement.assertOwned();
  assert.notDeepEqual(await readdir(lockPath), [ownerName]);
  await replacement.release();
  await replacement.release();
  await assert.rejects(stat(lockPath), { code: "ENOENT" });
});

test("waits for a live local owner and refreshes its heartbeat", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-live-owner-");
  const lockPath = join(root, "snapshot.lock");
  let now = Date.now();
  const lockOptions = options({
    staleMs: 10_000,
    dependencies: { now: () => now, setTimeout },
  });
  const owner = await acquireModelArtifactCacheLock(lockPath, lockOptions);
  now += 9_000;
  await owner.touch();
  now += 9_000;
  await assertWaitsForOwner(lockPath, lockOptions);
  await owner.assertOwned();
  await owner.release();
});

test("retains the TTL for foreign, malformed and partially initialized owners", async (t) => {
  const cases = [
    [
      "foreign hostname",
      {
        token: "existing-owner",
        pid: process.pid,
        hostname: `${hostname()}-other`,
      },
    ],
    ["JSON null", "null"],
    ["incomplete JSON", "{"],
    ["unknown hostname", { token: "existing-owner", pid: process.pid }],
    ["invalid PID", { token: "existing-owner", pid: -1, hostname: hostname() }],
    [
      "mismatched owner token",
      { token: "different-owner", pid: process.pid, hostname: hostname() },
    ],
    ["no owner file", undefined],
  ];
  for (const [name, value] of cases) {
    await t.test(name, async (t) => {
      const root = await createTemporaryDirectory(
        t,
        "zvec-artifact-unknown-owner-",
      );
      const lockPath = join(root, "snapshot.lock");
      if (value === undefined) {
        await mkdir(lockPath);
      } else {
        await writeOwner(lockPath, value);
      }
      t.mock.method(process, "kill", () => {
        assert.fail(
          "unknown and foreign owners must not be probed as local PIDs",
        );
      });
      await assertWaitsForOwner(lockPath);
      const now = Date.now() + 10 * 60 * 1_000 + 1_000;
      const replacement = await acquireModelArtifactCacheLock(
        lockPath,
        options({ dependencies: { now: () => now, setTimeout } }),
      );
      await replacement.assertOwned();
      await replacement.release();
    });
  }
});

test("treats EPERM and unknown process-probe errors conservatively", async (t) => {
  for (const code of ["EPERM", "EINVAL"]) {
    await t.test(code, async (t) => {
      const root = await createTemporaryDirectory(
        t,
        "zvec-artifact-probe-error-",
      );
      const lockPath = join(root, "snapshot.lock");
      await writeOwner(lockPath, {
        token: "existing-owner",
        pid: process.pid,
        hostname: hostname(),
      });
      t.mock.method(process, "kill", () => {
        throw Object.assign(new Error("unable to inspect process"), { code });
      });
      await assertWaitsForOwner(lockPath);
    });
  }
});

test("fences an expired owner and preserves its successor during release", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-artifact-lock-fence-");
  const lockPath = join(root, "snapshot.lock");
  let now = Date.now();
  const lockOptions = options({
    dependencies: { now: () => now, setTimeout },
  });
  const original = await acquireModelArtifactCacheLock(lockPath, lockOptions);
  now += 10 * 60 * 1_000 + 1_000;
  const replacement = await acquireModelArtifactCacheLock(
    lockPath,
    lockOptions,
  );
  const replacementFiles = await readdir(lockPath);
  await assert.rejects(original.assertOwned(), { code: "ENOENT" });
  await assert.rejects(original.touch(), { code: "ENOENT" });
  await original.release();
  assert.deepEqual(await readdir(lockPath), replacementFiles);
  await replacement.assertOwned();
  await replacement.release();
});
