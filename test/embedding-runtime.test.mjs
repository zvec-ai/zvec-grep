import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { readWorkspaceManifest } from "../dist/engine/manifest.js";
import { updateGlobalConfig } from "../dist/engine/config.js";
import { createZvecGrep } from "../dist/index.js";
import {
  createRemoteEmbeddingOperationPermit,
  createRemoteEmbeddingTarget,
  withRemoteEmbeddingOperationPermit,
} from "../dist/authorization/index.js";
import { createFakeEmbeddingServer } from "./helpers/fake-embedding.mjs";

test("workspace runtime persists explicit key and endpoint but search overrides stay one-shot", async (t) => {
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

  assert.deepEqual(readRuntime(root), {
    apiKey: "workspace-key",
    endpoint,
  });

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
  assert.deepEqual(readRuntime(root), {
    apiKey: "workspace-key",
    endpoint,
  });

  service = await createZvecGrep({
    root,
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
  assert.deepEqual(readRuntime(root), {
    apiKey: "workspace-key",
    endpoint,
  });

  service = await createZvecGrep({
    root,
    endpoint: replacementEndpoint,
  });
  await withPermit(root, replacementEndpoint, () =>
    service.index({ rebuild: true }),
  );
  await service.close();
  assert.deepEqual(readRuntime(root), {
    apiKey: "workspace-key",
    endpoint: replacementEndpoint,
  });
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

test("DGX no-auth configuration indexes and queries through the OpenAI-compatible endpoint", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-dgx-runtime-"),
  );
  const root = join(temporaryDirectory, "repo");
  const requests = [];
  const endpoint = await createFakeEmbeddingServer(t, 1024, {
    onRequest({ request, body }) {
      requests.push({
        authorization: request.headers.authorization,
        body,
      });
    },
  });
  const originalHome = process.env.HOME;
  const originalApiKey = process.env.ZVEC_GREP_API_KEY;
  process.env.HOME = temporaryDirectory;
  process.env.ZVEC_GREP_API_KEY = "unrelated-environment-key";
  t.after(async () => {
    if (originalHome === undefined) delete process.env.HOME;
    else process.env.HOME = originalHome;
    if (originalApiKey === undefined) delete process.env.ZVEC_GREP_API_KEY;
    else process.env.ZVEC_GREP_API_KEY = originalApiKey;
    await rm(temporaryDirectory, { recursive: true, force: true });
  });
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "example.ts"), "export const spark = 'ready';\n");
  updateGlobalConfig({
    defaults: { embedding: "dgx/qwen3-embedding-0.6b" },
    providers: { dgx: { auth: "none" } },
    models: {
      "dgx/qwen3-embedding-0.6b": { endpoint },
    },
  });

  const service = await createZvecGrep({ root });
  t.after(() => service.close());
  await withPermit(
    root,
    endpoint,
    () => service.index(),
    "dgx",
    "qwen3-embedding:0.6b",
  );
  const info = await service.info({ includeStatus: false });
  assert.deepEqual(info.workspaceIndex.embedding, {
    provider: "dgx",
    model: "qwen3-embedding:0.6b",
    dimension: 1024,
    metric: "cosine",
  });
  assert.deepEqual(readRuntime(root), { endpoint });

  const result = await withPermit(
    root,
    endpoint,
    () =>
      service.context({
        root,
        query: "spark readiness",
        autoUpdate: false,
      }),
    "dgx",
    "qwen3-embedding:0.6b",
  );
  assert.equal(result.source, "index");
  assert.ok(requests.length >= 2);
  assert.ok(requests.every((request) => request.authorization === undefined));
  assert.ok(
    requests.every((request) => request.body.model === "qwen3-embedding:0.6b"),
  );
  assert.ok(
    requests.every(
      (request) =>
        !("dimensions" in request.body) && !("encoding_format" in request.body),
    ),
  );
});

function readRuntime(root) {
  return readWorkspaceManifest(join(root, ".zvec-grep")).embeddingRuntime;
}

async function withPermit(
  root,
  endpoint,
  operation,
  provider = "qwen",
  model = "text-embedding-v4",
) {
  const target = await createRemoteEmbeddingTarget({
    roots: [root],
    provider,
    model,
    endpoint,
  });
  return await withRemoteEmbeddingOperationPermit(
    createRemoteEmbeddingOperationPermit(target, "once"),
    operation,
  );
}
