import assert from "node:assert/strict";
import {
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  RemoteEmbeddingAuthorizationManager,
  RemoteEmbeddingAuthorizationStore,
  createRemoteEmbeddingTarget,
  planRemoteIndexAuthorization,
  planRemoteSearchAuthorization,
  withRemoteEmbeddingOperationPermit,
} from "../dist/authorization/index.js";
import { createEmbeddingModelForIdentity } from "../dist/engine/service/zvec-grep.js";

test("Workspace Remote Embedding grants are signed, target-bound, and revocable", async (t) => {
  const temporaryDirectory = await mkdtemp(join(tmpdir(), "zg-auth-store-"));
  const rootA = join(temporaryDirectory, "a");
  const rootB = join(temporaryDirectory, "b");
  await Promise.all([mkdir(rootA), mkdir(rootB)]);
  const store = new RemoteEmbeddingAuthorizationStore({
    signingKeyPath: join(temporaryDirectory, "signing.key"),
  });
  const target = await createRemoteEmbeddingTarget({
    roots: [rootB, rootA, rootA],
    provider: "qwen",
    model: "text-embedding-v4",
    endpoint: "https://qwen.test/embeddings",
  });
  t.after(() => rm(temporaryDirectory, { recursive: true, force: true }));

  assert.deepEqual(target.workspaceRoots, [
    await realpath(rootA),
    await realpath(rootB),
  ]);
  await store.grant(target);
  assert.equal(await store.hasGrant(target), true);
  const status = await store.status(rootA);
  assert.equal(status.grants.length, 1);
  assert.equal(status.grants[0].valid, true);
  assert.equal(status.grants[0].scope, "workspace");

  const document = JSON.parse(await readFile(status.path, "utf8"));
  document.grants[0].endpoint = "https://tampered.test/embeddings";
  await writeFile(status.path, `${JSON.stringify(document, null, 2)}\n`);
  assert.equal(await store.hasGrant(target), false);

  await store.grant(target);
  assert.equal(await store.revokeAll(rootB), 1);
  assert.equal(await store.hasGrant(target), false);
});

test("remote provider guard fails closed and re-checks Workspace revocation", async (t) => {
  const temporaryDirectory = await mkdtemp(join(tmpdir(), "zg-auth-guard-"));
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const endpoint = "https://qwen.test/embeddings";
  const store = new RemoteEmbeddingAuthorizationStore({
    signingKeyPath: join(temporaryDirectory, "signing.key"),
  });
  const manager = new RemoteEmbeddingAuthorizationManager(store);
  const target = await createRemoteEmbeddingTarget({
    roots: [root],
    provider: "qwen",
    model: "text-embedding-v4",
    endpoint,
  });
  const plan = {
    operation: "query",
    target,
    disclosure: { queryText: true, workspaceContent: "none" },
    reason: "query",
    grantPath: store.grantPath(target),
  };
  const originalFetch = globalThis.fetch;
  let fetches = 0;
  globalThis.fetch = async () => {
    fetches += 1;
    return new Response(
      JSON.stringify({
        data: [{ index: 0, embedding: new Array(1024).fill(0.01) }],
      }),
      { status: 200, headers: { "Content-Type": "application/json" } },
    );
  };
  const model = createEmbeddingModelForIdentity(
    { provider: "qwen", name: "text-embedding-v4" },
    {
      apiKey: "test-key",
      endpoint,
      authorizationSigningKeyPath: join(temporaryDirectory, "signing.key"),
    },
  );
  t.after(async () => {
    globalThis.fetch = originalFetch;
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await assert.rejects(
    model.embed([{ kind: "text", text: "not authorized" }], {
      purpose: "query",
    }),
    /authorization is required/i,
  );
  assert.equal(fetches, 0);

  const once = await manager.grant(plan, "once");
  assert.equal(await store.hasGrant(target), false);
  await withRemoteEmbeddingOperationPermit(once, () =>
    model.embed([{ kind: "text", text: "once" }], { purpose: "query" }),
  );
  assert.equal(fetches, 1);

  const workspace = await manager.grant(plan, "workspace");
  await withRemoteEmbeddingOperationPermit(workspace, () =>
    model.embed([{ kind: "text", text: "workspace" }], {
      purpose: "query",
    }),
  );
  assert.equal(fetches, 2);
  await store.revoke(target);
  await assert.rejects(
    withRemoteEmbeddingOperationPermit(workspace, () =>
      model.embed([{ kind: "text", text: "revoked" }], {
        purpose: "query",
      }),
    ),
    /authorization is required/i,
  );
  assert.equal(fetches, 2);
});

test("authorization planner follows merged Query and Index behavior", async (t) => {
  const temporaryDirectory = await mkdtemp(join(tmpdir(), "zg-auth-plan-"));
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  t.after(() => rm(temporaryDirectory, { recursive: true, force: true }));
  const schema = {
    provider: "qwen",
    model: "text-embedding-v4",
    dimension: 1024,
    metric: "cosine",
  };
  const freshStatus = status(0);
  const staleStatus = status(1);
  const baseInfo = {
    root,
    indexed: true,
    indexPolicy: "enabled",
    home: join(root, ".zvec-grep"),
    indexPath: join(root, ".zvec-grep", "index"),
    source: "index",
    collection: {
      id: "collection",
      name: "__anonymous__",
      path: join(root, ".zvec-grep", "index"),
      rootPaths: [{ absolutePath: root, recursive: true }],
      embedding: schema,
      indexVersion: 1,
      createdTime: 1,
      updatedTime: 1,
    },
  };
  const hybrid = {
    root,
    queries: ["authorization"],
    routes: [],
    freshness: "eventual",
    autoUpdate: true,
  };
  const fts = {
    ...hybrid,
    queries: undefined,
    routes: [{ mode: "fts", query: "authorization" }],
  };

  const queryPlan = await planRemoteSearchAuthorization({
    info: { ...baseInfo, status: freshStatus },
    search: hybrid,
  });
  assert.equal(queryPlan.operation, "query");
  assert.deepEqual(queryPlan.disclosure, {
    queryText: true,
    workspaceContent: "none",
  });

  const coupledFtsPlan = await planRemoteSearchAuthorization({
    info: { ...baseInfo, status: staleStatus },
    search: fts,
  });
  assert.equal(coupledFtsPlan.operation, "index");
  assert.deepEqual(coupledFtsPlan.disclosure, {
    queryText: false,
    workspaceContent: "changed",
  });

  assert.equal(
    await planRemoteIndexAuthorization({
      info: { ...baseInfo, status: freshStatus },
      schema,
    }),
    undefined,
  );
  const updatePlan = await planRemoteIndexAuthorization({
    info: { ...baseInfo, status: staleStatus },
    schema,
  });
  assert.equal(updatePlan.operation, "index");
  assert.equal(updatePlan.reason, "index_update");
});

function status(filesModified) {
  return {
    filesStored: 1,
    filesScanned: 1,
    filesAdded: 0,
    filesModified,
    filesDeleted: 0,
    filesIndexed: 1,
    filesPending: 0,
    filesFailed: 0,
    entitiesIndexed: 1,
  };
}
