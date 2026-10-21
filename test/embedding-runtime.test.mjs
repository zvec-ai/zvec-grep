import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { readWorkspaceManifest } from "../dist/engine/manifest.js";
import { updateGlobalConfig } from "../dist/engine/config.js";
import { createZvecGrep } from "../dist/index.js";
import { useIsolatedZvecGrepHome } from "./helpers/isolated-home.mjs";
import {
  createRemoteEmbeddingOperationPermit,
  createRemoteEmbeddingTarget,
  withRemoteEmbeddingOperationPermit,
} from "../dist/authorization/index.js";
import { createFakeEmbeddingServer } from "./helpers/fake-embedding.mjs";

useIsolatedZvecGrepHome();

test("workspace runtime persists the endpoint but never the key", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-workspace-runtime-"),
  );
  const root = join(temporaryDirectory, "repo");
  const endpoint = await createFakeEmbeddingServer(t);
  const replacementEndpoint = await createFakeEmbeddingServer(t);
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "example.ts"), "export const answer = 42;\n");

  let service = await createZvecGrep({
    root,
    embedding: "qwen/text-embedding-v4",
    apiKey: "workspace-key",
    endpoint,
  });
  await withPermit(root, endpoint, () => service.index());
  const publicInfo = await service.info({ includeStatus: false });
  assert.ok(publicInfo.workspaceIndex);
  assert.equal("embeddingRuntime" in publicInfo.workspaceIndex, false);
  assert.equal("manifestVersion" in publicInfo.workspaceIndex, false);
  assert.doesNotMatch(JSON.stringify(publicInfo), /workspace-key/);
  await service.close();

  // Only the endpoint is portable identity; the key is never persisted.
  assert.deepEqual(readRuntime(root), { endpoint });

  // A later session without credentials cannot use the remote provider...
  service = await createZvecGrep({ root });
  await assert.rejects(
    service.context({
      root,
      query: "where is answer defined",
      autoUpdate: false,
    }),
    /api key/i,
  );
  await service.close();
  assert.deepEqual(readRuntime(root), { endpoint });

  // ...but a session-scoped key works and is still not persisted.
  service = await createZvecGrep({
    root,
    apiKey: "one-shot-key",
  });
  await withPermit(root, endpoint, () =>
    service.context({
      root,
      query: "where is answer defined",
      autoUpdate: false,
    }),
  );
  await service.close();
  assert.deepEqual(readRuntime(root), { endpoint });

  service = await createZvecGrep({
    root,
    apiKey: "one-shot-key",
    endpoint: replacementEndpoint,
  });
  await assert.rejects(
    service.context({
      root,
      query: "where is answer defined",
      autoUpdate: false,
    }),
    /cannot override.*endpoint/i,
  );
  await assert.rejects(service.index(), /different embedding endpoint/i);
  await service.close();
  assert.deepEqual(readRuntime(root), { endpoint });

  service = await createZvecGrep({
    root,
    apiKey: "workspace-key",
    endpoint: replacementEndpoint,
  });
  await withPermit(root, replacementEndpoint, () =>
    service.index({ rebuild: true }),
  );
  await service.close();
  assert.deepEqual(readRuntime(root), { endpoint: replacementEndpoint });
});

test("inherited provider keys are not copied into workspace metadata", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-inherited-runtime-"),
  );
  const root = join(temporaryDirectory, "repo");
  const endpoint = await createFakeEmbeddingServer(t);
  const originalHome = process.env.HOME;
  process.env.HOME = temporaryDirectory;
  t.after(async () => {
    if (originalHome === undefined) delete process.env.HOME;
    else process.env.HOME = originalHome;
    await rm(temporaryDirectory, { recursive: true, force: true });
  });
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "example.ts"), "export const answer = 42;\n");
  updateGlobalConfig({
    defaults: { embedding: "qwen/text-embedding-v4" },
    providers: { qwen: { apiKey: "global-key" } },
    models: {
      "qwen/text-embedding-v4": { endpoint },
    },
  });

  const service = await createZvecGrep({ root });
  await withPermit(root, endpoint, () => service.index());
  await service.close();

  assert.deepEqual(readRuntime(root), { endpoint });
});

function readRuntime(root) {
  return readWorkspaceManifest(join(root, ".zvec-grep")).embeddingRuntime;
}

async function withPermit(root, endpoint, operation) {
  const target = await createRemoteEmbeddingTarget({
    roots: [root],
    provider: "qwen",
    model: "text-embedding-v4",
    endpoint,
  });
  return await withRemoteEmbeddingOperationPermit(
    createRemoteEmbeddingOperationPermit(target, "once"),
    operation,
  );
}
