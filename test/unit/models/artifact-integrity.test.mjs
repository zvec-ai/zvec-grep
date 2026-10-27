import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import {
  mkdir,
  readFile,
  readdir,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { acquireModelArtifactCacheLock } from "../../../dist/engine/models/artifact-cache-lock.js";
import { resolveModelArtifacts } from "../../../dist/engine/models/artifact-downloader.js";
import { createTemporaryDirectory } from "../../helpers/fixtures.mjs";

const bytes = Buffer.from("verified model artifact");
const artifact = {
  path: "model.onnx",
  size: bytes.length,
  sha256: createHash("sha256").update(bytes).digest("hex"),
};

function options(directory, kind = "huggingface") {
  return {
    model: "local/test-model",
    sources: [
      {
        kind,
        repo: "owner/model",
        revision: "pinned",
        cacheDirectory: directory,
      },
    ],
    artifacts: [artifact],
    lock: { pollMs: 2, staleMs: 10_000, heartbeatMs: 100 },
  };
}

async function markerPath(directory) {
  const names = (await readdir(directory)).filter((name) =>
    name.endsWith(".complete"),
  );
  assert.equal(names.length, 1);
  return join(directory, names[0]);
}

async function matchMarkerToFile(directory) {
  // Construct matching metadata deterministically, including ctime, instead of
  // depending on the filesystem's timestamp resolution or a timed sleep.
  const path = await markerPath(directory);
  const marker = JSON.parse(await readFile(path, "utf8"));
  const stats = await stat(join(directory, artifact.path));
  marker.files[artifact.path] = {
    size: stats.size,
    mtimeMs: stats.mtimeMs,
    ctimeMs: stats.ctimeMs,
  };
  await writeFile(path, JSON.stringify(marker));
}

for (const kind of ["huggingface", "modelscope"]) {
  test(`hashes ${kind} cache contents even when completion metadata matches`, async (t) => {
    const directory = await createTemporaryDirectory(t, "zvec-artifact-hash-");
    const destination = join(directory, artifact.path);
    await writeFile(destination, bytes);
    let fetchCalls = 0;
    const resolveOptions = {
      ...options(directory, kind),
      dependencies: {
        async fetch() {
          fetchCalls++;
          return new Response(bytes);
        },
      },
    };
    await resolveModelArtifacts(resolveOptions);
    await resolveModelArtifacts(resolveOptions);
    assert.equal(fetchCalls, 0, "valid cached contents require no network");

    await writeFile(destination, Buffer.alloc(bytes.length, 120));
    await matchMarkerToFile(directory);
    await resolveModelArtifacts(resolveOptions);

    assert.equal(fetchCalls, 1, "matching metadata cannot certify contents");
    assert.deepEqual(await readFile(destination), bytes);
    await resolveModelArtifacts(resolveOptions);
    assert.equal(fetchCalls, 1, "the repaired cache remains usable offline");
  });
}

for (const corrupt of [false, true]) {
  test(
    `verifies a ${corrupt ? "corrupt" : "valid"} snapshot published while waiting for the lock`,
    { timeout: 10_000 },
    async (t) => {
      const directory = await createTemporaryDirectory(
        t,
        "zvec-artifact-recheck-",
      );
      const destination = join(directory, artifact.path);
      await mkdir(directory, { recursive: true });
      await writeFile(destination, bytes);
      const resolveOptions = options(directory);
      await resolveModelArtifacts(resolveOptions);
      const completePath = await markerPath(directory);
      const lock = await acquireModelArtifactCacheLock(
        completePath.replace(/\.complete$/u, ".lock"),
        {
          ...resolveOptions.lock,
          dependencies: { now: Date.now, setTimeout },
        },
      );
      t.after(() => lock.release());
      await rm(destination);

      let notifyWaiting;
      const waiting = new Promise((resolve) => {
        notifyWaiting = resolve;
      });
      let fetchCalls = 0;
      const plans = [];
      const resolution = resolveModelArtifacts({
        ...resolveOptions,
        onDownloadPlan: (artifacts) => plans.push(artifacts),
        dependencies: {
          setTimeout(callback, milliseconds) {
            if (milliseconds === resolveOptions.lock.pollMs) {
              notifyWaiting();
            }
            return setTimeout(callback, milliseconds);
          },
          async fetch() {
            fetchCalls++;
            return new Response(bytes);
          },
        },
      });
      // The initial lookup has finished and this caller is waiting for the
      // other writer. Only now publish that writer's snapshot and marker.
      await waiting;
      await writeFile(
        destination,
        corrupt ? Buffer.alloc(bytes.length, 120) : bytes,
      );
      await matchMarkerToFile(directory);
      await lock.release();
      await resolution;

      assert.equal(fetchCalls, corrupt ? 1 : 0);
      assert.deepEqual(plans, corrupt ? [[artifact]] : []);
      assert.deepEqual(await readFile(destination), bytes);
    },
  );
}
