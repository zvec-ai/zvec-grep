import assert from "node:assert/strict";
import { chmod, mkdir, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import {
  readWorkspaceManifest,
  workspaceManifestPath,
  writeWorkspaceManifest,
} from "../../dist/engine/manifest.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

test("workspace manifest persists portable index metadata and embedding runtime", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-manifest-",
  );
  const home = join(temporaryDirectory, ".zvec-grep");
  const now = Date.now();
  const manifest = {
    manifestVersion: 2,
    id: "workspace-id",
    name: "repo",
    rootPaths: [{ path: ".", recursive: true }],
    indexPolicy: "enabled",
    embedding: {
      provider: "fake",
      model: "fake-model",
      dimension: 8,
      metric: "cosine",
    },
    indexVersion: 2,
    createdTime: now,
    updatedTime: now,
    embeddingRuntime: {
      endpoint: "https://example.test/embeddings",
    },
  };

  assert.equal(readWorkspaceManifest(home), null);
  writeWorkspaceManifest(home, manifest);
  assert.deepEqual(readWorkspaceManifest(home), manifest);

  if (process.platform !== "win32") {
    assert.equal((await stat(home)).mode & 0o777, 0o700);
    assert.equal((await stat(workspaceManifestPath(home))).mode & 0o777, 0o600);
  }
});

test("workspace manifest rejects persisted host bindings", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-manifest-bindings-",
  );
  const home = join(temporaryDirectory, ".zvec-grep");
  const now = Date.now();
  const base = {
    manifestVersion: 2,
    id: "workspace-id",
    name: "repo",
    rootPaths: [{ path: ".", recursive: true }],
    indexPolicy: "enabled",
    embedding: null,
    indexVersion: null,
    createdTime: now,
    updatedTime: now,
  };

  for (const embeddingRuntime of [{ apiKey: "secret" }, { device: "metal" }]) {
    writeWorkspaceManifest(home, { ...base, embeddingRuntime });
    assert.throws(
      () => readWorkspaceManifest(home),
      (error) => error.code === "ZVEC_GREP.ENGINE.MANIFEST.INVALID",
    );
  }
});

test("workspace manifest reports legacy format as needing migration", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-manifest-legacy-",
  );
  const home = join(temporaryDirectory, ".zvec-grep");
  const now = Date.now();
  writeWorkspaceManifest(home, {
    manifestVersion: 1,
    id: "legacy-id",
    name: "legacy",
    path: home,
    rootPaths: [{ absolutePath: temporaryDirectory, recursive: true }],
    indexPolicy: "enabled",
    embedding: null,
    indexVersion: null,
    createdTime: now,
    updatedTime: now,
    embeddingRuntime: {},
  });

  assert.throws(
    () => readWorkspaceManifest(home),
    (error) => error.code === "ZVEC_GREP.ENGINE.MANIFEST.MIGRATION_REQUIRED",
  );
});

test("workspace manifest rejects unsupported or malformed data", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-invalid-manifest-",
  );
  const home = join(temporaryDirectory, ".zvec-grep");
  await mkdir(home, { recursive: true });
  await chmod(home, 0o700);
  await writeFile(
    workspaceManifestPath(home),
    JSON.stringify({ manifestVersion: 999 }),
  );

  assert.throws(
    () => readWorkspaceManifest(home),
    (error) => error.code === "ZVEC_GREP.ENGINE.MANIFEST.INVALID",
  );
});
