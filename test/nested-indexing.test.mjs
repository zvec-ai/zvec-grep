import assert from "node:assert/strict";
import { access, mkdir, readFile, realpath, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { DaemonBackend } from "../dist/daemon/backend.js";
import { readWorkspaceManifest } from "../dist/engine/manifest.js";
import { createZvecGrep } from "../dist/index.js";
import { acquireReadWriteLock } from "../dist/engine/utils/lock.js";
import {
  FakeEmbeddingModel,
  createFakeEmbeddingServer,
} from "./helpers/fake-embedding.mjs";
import { createTemporaryDirectory, runCli } from "./helpers/fixtures.mjs";

const parentModel = "potion-retrieval-32m";
const childModel = "potion-code-16m-v2";

class NamedEmbeddingModel extends FakeEmbeddingModel {
  constructor({ provider = "local", name, endpoint }) {
    super();
    this.info = {
      ...this.info,
      provider,
      name,
      reference: `${provider}/${name}`,
      ...(endpoint ? { endpoint } : {}),
    };
  }
}

async function nestedWorkspace(t, identity = { name: parentModel }) {
  const root = await realpath(
    await createTemporaryDirectory(t, "zvec-grep-nested-index-"),
  );
  const child = join(root, "sub");
  await mkdir(join(root, "docs"));
  await mkdir(join(child, "src"), { recursive: true });
  await writeFile(join(root, "docs", "guide.md"), "# ParentGuideNeedle\n");
  await writeFile(join(root, "docs", "notes.md"), "# ParentNotesNeedle\n");
  await writeFile(
    join(child, "src", "parser.ts"),
    "export const ChildParserNeedle = 42;\n",
  );
  const service = await createZvecGrep({
    root,
    embeddingModel: new NamedEmbeddingModel(identity),
    ...(identity.endpoint
      ? { endpoint: identity.endpoint, apiKey: "parent-test-key" }
      : {}),
  });
  try {
    await service.index({ globs: ["docs/**"] });
    assert.equal((await service.info()).status.filesIndexed, 2);
  } finally {
    await service.close();
  }
  return {
    root,
    child,
    parentManifest: await readFile(
      join(root, ".zvec-grep", "manifest.json"),
      "utf8",
    ),
  };
}

async function infoAt(root, options = {}) {
  const service = await createZvecGrep({ root });
  try {
    return await service.info({ root, ...options });
  } finally {
    await service.close();
  }
}

async function assertParentUnchanged(workspace) {
  assert.equal(
    await readFile(join(workspace.root, ".zvec-grep", "manifest.json"), "utf8"),
    workspace.parentManifest,
  );
  const parent = await infoAt(workspace.root);
  assert.deepEqual(parent.workspaceIndex.rootPaths[0].globs, ["docs/**"]);
  assert.equal(parent.status.filesScanned, 2);
  assert.equal(parent.status.filesIndexed, 2);
}

function createBackend(workspace, overrides = {}) {
  return new DaemonBackend({
    version: "test",
    serviceOptions: {
      embedding: `local/${childModel}`,
      authorizationSigningKeyPath: join(workspace.root, "test-signing.key"),
    },
    modelPoolOptions: {
      maxLoadedModels: 2,
      createModel: ({ model, runtime }) =>
        new NamedEmbeddingModel({ ...model, endpoint: runtime?.endpoint }),
    },
    watchManagerFactory: () => ({
      start() {},
      flushPending: async () => {},
      close: async () => {},
    }),
    ...overrides,
  });
}

test("exact root info ignores ancestor metadata and locks", async (t) => {
  const workspace = await nestedWorkspace(t);
  const inherited = await infoAt(workspace.child, { includeStatus: false });
  assert.equal(inherited.root, workspace.root);
  const parentLock = acquireReadWriteLock(
    join(workspace.root, ".zvec-grep", "locks", "home"),
    "write",
    { operation: "test.parent.index" },
  );
  try {
    const exact = await infoAt(workspace.child, { exactRoot: true });
    assert.equal(exact.root, workspace.child);
    assert.equal(exact.indexed, false);
    assert.equal(exact.workspaceIndex, undefined);
    assert.equal(exact.home, join(workspace.child, ".zvec-grep"));
    await assert.rejects(infoAt(workspace.child), {
      code: "ZVEC_GREP.ENGINE.LOCK.BUSY",
    });
  } finally {
    parentLock.release();
  }
  const childLock = acquireReadWriteLock(
    join(workspace.child, ".zvec-grep", "locks", "home"),
    "write",
    { operation: "test.child.index" },
  );
  try {
    await assert.rejects(infoAt(workspace.child, { exactRoot: true }), {
      code: "ZVEC_GREP.ENGINE.LOCK.BUSY",
    });
  } finally {
    childLock.release();
  }
});

test("direct CLI creates a nested index with a different model without rebuild", async (t) => {
  const workspace = await nestedWorkspace(t);
  const endpoint = await createFakeEmbeddingServer(t);
  const result = await runCli(
    [
      "--index",
      workspace.child,
      "--mode",
      "direct",
      "--embedding",
      "qwen/text-embedding-v4",
      "--api-key",
      "child-test-key",
      "--endpoint",
      endpoint,
      "--allow-remote",
      "-g",
      "src/**",
    ],
    {
      env: {
        ZVEC_GREP_AUTHORIZATION_KEY_FILE: join(
          workspace.root,
          "test-signing.key",
        ),
      },
    },
  );
  assert.match(result.stdout, /Workspace index/);
  const child = await infoAt(workspace.child, { exactRoot: true });
  assert.equal(child.root, workspace.child);
  assert.equal(child.workspaceIndex.embedding.model, "text-embedding-v4");
  assert.equal(child.status.filesIndexed, 1);
  const manifest = readWorkspaceManifest(child.home);
  assert.equal(manifest.embeddingRuntime.apiKey, "child-test-key");
  assert.equal(manifest.embeddingRuntime.endpoint, endpoint);
  await assertParentUnchanged(workspace);

  await assert.rejects(
    runCli([
      "--index",
      workspace.child,
      "--mode",
      "direct",
      "--embedding",
      `local/${parentModel}`,
    ]),
    (error) => {
      assert.match(error.stderr, /Embedding model does not match/);
      assert.match(error.stderr, /Existing model: qwen\/text-embedding-v4/);
      return true;
    },
  );
});

test("daemon index honors a nested root for defaults, matching models, and rebuilds", async (t) => {
  const cases = [
    { name: "new-index server default", expectedModel: childModel },
    { name: "same model", embedding: parentModel, expectedModel: parentModel },
    {
      name: "different model",
      embedding: childModel,
      expectedModel: childModel,
    },
    {
      name: "different model with rebuild",
      embedding: childModel,
      expectedModel: childModel,
      rebuild: true,
    },
    { name: "cached query alias", expectedModel: childModel, queryFirst: true },
  ];
  for (const scenario of cases) {
    await t.test(scenario.name, async (t) => {
      const workspace = await nestedWorkspace(t);
      const backend = createBackend(workspace);
      try {
        if (scenario.queryFirst) {
          const result = await backend.search({
            root: workspace.child,
            routes: [{ mode: "fts", query: "ParentGuideNeedle" }],
            freshness: "eventual",
            autoUpdate: false,
          });
          assert.match(result.result.items[0].content, /ParentGuideNeedle/);
        }
        const input = {
          root: workspace.child,
          globs: ["src/**"],
          wait: true,
          ...(scenario.embedding
            ? { embedding: `local/${scenario.embedding}` }
            : {}),
          ...(scenario.rebuild ? { rebuild: true } : {}),
        };
        assert.equal(await backend.planIndexAuthorization(input), undefined);
        const result = await backend.index(input);
        assert.equal(result.state, "succeeded", JSON.stringify(result.error));
        assert.equal(result.root, workspace.child);
        const child = await infoAt(workspace.child, { exactRoot: true });
        assert.equal(
          child.workspaceIndex.embedding.model,
          scenario.expectedModel,
        );
        assert.deepEqual(child.workspaceIndex.rootPaths[0].globs, ["src/**"]);
        assert.equal(child.status.filesScanned, 1);
        assert.equal(child.status.filesIndexed, 1);
        await assertParentUnchanged(workspace);
        const query = await backend.search({
          root: workspace.child,
          routes: [{ mode: "fts", query: "ChildParserNeedle" }],
          freshness: "eventual",
          autoUpdate: false,
        });
        assert.match(query.result.items[0].content, /ChildParserNeedle/);
        const parentQuery = await backend.search({
          root: workspace.root,
          routes: [{ mode: "fts", query: "ParentGuideNeedle" }],
          freshness: "eventual",
          autoUpdate: false,
        });
        assert.match(parentQuery.result.items[0].content, /ParentGuideNeedle/);
      } finally {
        await backend.close();
      }
    });
  }
});

test("daemon model checks and remote authorization use the exact target manifest", async (t) => {
  const workspace = await nestedWorkspace(t, {
    provider: "qwen",
    name: "text-embedding-v4",
    endpoint: "https://parent.example/embeddings",
  });
  const backend = createBackend(workspace);
  try {
    const plan = await backend.planIndexAuthorization({
      root: workspace.child,
      embedding: "qwen/text-embedding-v4",
      apiKey: "child-test-key",
      endpoint: "https://child.example/embeddings",
    });
    assert.equal(plan.reason, "index_create");
    assert.deepEqual(plan.target.workspaceRoots, [workspace.child]);
    assert.equal(plan.target.endpoint, "https://child.example/embeddings");
    assert.equal(
      plan.grantPath,
      join(workspace.child, ".zvec-grep", "authorization.json"),
    );
    await assert.rejects(
      backend.planIndexAuthorization({
        root: workspace.root,
        embedding: `local/${childModel}`,
      }),
      /EMBEDDING_MODEL_MISMATCH/,
    );
    await assert.rejects(
      backend.planIndexAuthorization({
        root: workspace.root,
        embedding: "qwen/text-embedding-v4",
        endpoint: "https://child.example/embeddings",
      }),
      /EMBEDDING_ENDPOINT_MISMATCH/,
    );
    await assert.rejects(
      access(join(workspace.child, ".zvec-grep", "manifest.json")),
      { code: "ENOENT" },
    );
    await assertParentUnchanged(workspace);
  } finally {
    await backend.close();
  }
});
