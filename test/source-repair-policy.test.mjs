import assert from "node:assert/strict";
import { mkdir, mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  RemoteEmbeddingAuthorizationStore,
  indexStatusIsFresh,
  planRemoteSearchAuthorization,
  remoteEmbeddingAuthorizationGuard,
} from "../dist/authorization/index.js";
import { DaemonBackend } from "../dist/daemon/backend.js";
import { DaemonError } from "../dist/daemon/errors.js";
import { RootRuntime } from "../dist/daemon/root-runtime.js";
import { preflightSearchPlan } from "../dist/engine/pipeline/search/index.js";
import { CURRENT_INDEX_VERSION } from "../dist/engine/types.js";
import { FakeEmbeddingModel } from "./helpers/fake-embedding.mjs";

test("FTS source repair authorization does not load a remote model with a missing API key", async (t) => {
  const rig = await fixture(t, {
    createModel: () => {
      throw new Error("fixture qwen API key is required");
    },
  });
  const result = await bounded(
    rig.backend.search({ ...ftsInput(rig.root), autoUpdate: true }),
    "local FTS was blocked by automatic repair authorization",
  );
  assert.equal(result.freshness, "possibly_stale");
  assert.equal(result.result.items[0].content, "indexed source content");
  assert.equal(result.result.items[0].status, "possibly_stale");
  assert.equal(rig.runtime.sourceInvalidations.hasPending(), true);
  assert.ok(rig.calls.authorizationChecks > 0, "the real grant policy ran");
  assert.equal(rig.calls.modelLoads, 0);
  assert.equal(rig.calls.queryEmbeddings, 0);
  assert.equal(rig.calls.documentEmbeddings, 0);
  assertNoJob(rig);
});

test("FTS source repair authorization returns before any held remote model load", async (t) => {
  const entered = deferred();
  const released = deferred();
  const rig = await fixture(t, {
    createModel: async (_request, createDefaultModel) => {
      entered.resolve();
      await released.promise;
      return createDefaultModel();
    },
  });
  const searching = rig.backend.search({
    ...ftsInput(rig.root),
    autoUpdate: true,
  });
  void searching.catch(() => {});
  try {
    assert.equal(
      await bounded(
        Promise.race([
          searching.then(() => "local_result"),
          entered.promise.then(() => "model_load"),
        ]),
        "FTS neither returned nor reached the unexpected model gate",
      ),
      "local_result",
      "a completed FTS result must not wait for remote model construction",
    );
    const result = await searching;
    assert.equal(result.freshness, "possibly_stale");
    assert.equal(result.result.items[0].content, "indexed source content");
    assert.equal(result.result.items[0].status, "possibly_stale");
    assert.equal(rig.runtime.sourceInvalidations.hasPending(), true);
    assert.ok(rig.calls.authorizationChecks > 0, "the real grant policy ran");
    assert.equal(rig.calls.modelLoads, 0);
    assert.equal(rig.calls.queryEmbeddings, 0);
    assert.equal(rig.calls.documentEmbeddings, 0);
    assertNoJob(rig);
  } finally {
    released.resolve();
    await bounded(
      Promise.allSettled([searching]),
      "released unexpected model load did not drain",
    );
  }
});

test("source repair preparation failure preserves eventual FTS but remains an explicit wait error", async (t) => {
  const rig = await fixture(t, { provider: "local" });
  const failure = new Error("fixture automatic repair preparation failed");
  const authorization = t.mock.method(
    rig.backend,
    "automaticIndexAuthorization",
    async () => {
      throw failure;
    },
  );
  const result = await bounded(
    rig.backend.search({ ...ftsInput(rig.root), autoUpdate: true }),
    "eventual FTS was blocked by failed background preparation",
  );
  assert.equal(result.freshness, "possibly_stale");
  assert.equal(result.result.items[0].content, "indexed source content");
  assert.equal(result.result.items[0].status, "possibly_stale");
  assert.equal(authorization.mock.callCount(), 1);
  assert.equal(rig.runtime.sourceInvalidations.hasPending(), true);
  assertNoJob(rig);

  await assert.rejects(
    bounded(
      rig.backend.search({
        ...ftsInput(rig.root),
        freshness: "wait_for_fresh",
        autoUpdate: true,
      }),
      "explicit freshness wait did not report failed preparation",
    ),
    (error) => error === failure,
  );
  assert.equal(authorization.mock.callCount(), 2);
  assert.equal(rig.runtime.sourceInvalidations.hasPending(), true);
  assert.equal(rig.runtime.sourceInvalidations.unattempted().length, 1);
  assert.equal(rig.runtime.snapshot().activeOperations, 0);
  assert.equal(rig.calls.modelLoads, 0);
  assertNoJob(rig);
});

test("query-only once permission cannot authorize eventual source repair", async (t) => {
  const rig = await fixture(t);
  const authorization = await rig.queryOnlyPermit();
  for (let attempt = 0; attempt < 2; attempt++) {
    const result = await bounded(
      rig.backend.search(ordinaryInput(rig.root), { authorization }),
      "eventual query waited for unauthorized source repair",
    );
    assert.equal(result.freshness, "possibly_stale");
    assert.equal(result.result.items[0].status, "possibly_stale");
    assertNoJob(rig);
  }
  assert.equal(rig.calls.queryEmbeddings, 2, "the query permit itself works");
  assert.equal(rig.calls.documentEmbeddings, 0);
  assert.equal(rig.calls.services, 0);
  assert.equal(rig.runtime.needsReconciliation(), true);
  assert.equal(rig.runtime.sourceInvalidations.pending().length, 1);
  assert.equal(
    rig.runtime.sourceInvalidations.unattempted().length,
    1,
    "denied automatic work must not consume the pending repair attempt",
  );
  assert.equal(await rig.store.hasGrant(authorization.target), false);
});

test("freshness wait rejects source repair without a workspace grant even with a query-only permit", async (t) => {
  const rig = await fixture(t);
  const authorization = await rig.queryOnlyPermit();
  await assert.rejects(
    bounded(
      rig.backend.search(
        { ...ordinaryInput(rig.root), freshness: "wait_for_fresh" },
        { authorization },
      ),
      "freshness wait did not reject unauthorized source repair",
    ),
    (error) => {
      assert.equal(error.code, "REMOTE_EMBEDDING_AUTH_REQUIRED");
      assert.match(error.message, /Workspace.*grant/i);
      return true;
    },
  );
  assert.equal(rig.calls.queryEmbeddings, 1);
  assert.equal(rig.calls.documentEmbeddings, 0);
  assert.equal(rig.calls.services, 0);
  assert.equal(rig.runtime.sourceInvalidations.hasPending(), true);
  assertNoJob(rig);
});

test("a fresh full metadata probe cannot erase search-discovered source invalidation", async (t) => {
  const rig = await fixture(t, { provider: "local" });
  await rig.backend.search(ftsInput(rig.root));
  const captured = rig.runtime.sourceInvalidations.pending();
  assert.equal(captured.length, 1);
  rig.runtime.requireFullReconciliation(true);
  const fullScansBefore = rig.calls.fullScans;
  const freshness = await rig.runtime.probeFreshness(async () =>
    indexStatusIsFresh(await rig.inspectRoot(rig.root, undefined, true)),
  );
  assert.equal(rig.calls.fullScans, fullScansBefore + 1);
  assert.equal(freshness, "stale");
  assert.equal(rig.runtime.requiresFullReconciliation(), false);
  assert.equal(rig.runtime.needsReconciliation(), true);
  assert.deepEqual(rig.runtime.sourceInvalidations.pending(), captured);

  // This next query does not rediscover the file: outer freshness must come
  // from the retained evidence, not merely from a stale item in this response.
  rig.state.reportInvalidation = false;
  const result = await rig.backend.search(ftsInput(rig.root));
  assert.deepEqual(result.result.items, []);
  assert.equal(result.freshness, "possibly_stale");
  assertNoJob(rig);
});

test("a successful index job with only unrelated path proof cannot clear pending source invalidation", async (t) => {
  const rig = await fixture(t, { provider: "local" });
  await rig.backend.search(ftsInput(rig.root));
  const captured = rig.runtime.sourceInvalidations.pending();
  assert.equal(captured.length, 1);
  const result = await bounded(
    rig.backend.index({ root: rig.root, wait: true }),
    "mock index job did not complete",
  );
  assert.equal(result.state, "succeeded");
  assert.equal(rig.calls.indexInputs.length, 1);
  assert.deepEqual(rig.calls.indexInputs[0].verifySourcePaths, [rig.source]);
  assert.deepEqual(rig.runtime.sourceInvalidations.pending(), captured);
  assert.equal(rig.runtime.needsReconciliation(), true);
  assert.equal(rig.runtime.requiresFullReconciliation(), false);
  rig.state.reportInvalidation = false;
  const later = await rig.backend.search(ftsInput(rig.root));
  assert.deepEqual(later.result.items, []);
  assert.equal(later.freshness, "possibly_stale");
  assert.equal(rig.backend.scheduler.hasActiveRoot(rig.root), false);
});

test("source-repair jobs retain runtime activity through coalesced followup and retry backoff", async (t) => {
  const entered = deferred();
  const release = deferred();
  const retrying = deferred();
  t.after(() => release.resolve());
  const rig = await fixture(t, {
    provider: "local",
    schedulerOptions: { maxAttempts: 2, retryBaseDelayMs: 60_000 },
    onEvent: (name) => {
      if (name === "job.retry") retrying.resolve();
    },
    index: async () => {
      entered.resolve();
      await release.promise;
      throw new DaemonError("FIXTURE_RETRY", "retryable mock repair", true);
    },
  });
  await bounded(
    rig.backend.search({ ...ftsInput(rig.root), autoUpdate: true }),
    "eventual search waited for its repair",
  );
  await bounded(entered.promise, "source repair did not enter the mock writer");
  const first = rig.backend.scheduler.getByRoot(rig.root);
  assert.equal(first.state, "running");
  assert.equal(rig.runtime.snapshot().activeOperations, 2);

  const submitted = rig.calls.submissions;
  await rig.watcher().onChanges(changes(join(rig.root, "other.ts")), "watch");
  const followupId = submitted.at(-1).job.id;
  await rig.watcher().onChanges(changes(join(rig.root, "third.ts")), "watch");
  assert.equal(submitted.at(-1).job.id, followupId);
  assert.notEqual(followupId, first.id);
  assert.equal(rig.backend.scheduler.get(followupId).state, "queued");
  assert.equal(
    rig.runtime.snapshot().activeOperations,
    3,
    "one running operation and two distinct jobs are retained; reuse adds no lease",
  );

  release.resolve();
  await bounded(retrying.promise, "repair did not enter retry backoff");
  assert.equal(rig.backend.scheduler.get(first.id).state, "queued");
  assert.equal(rig.backend.scheduler.get(followupId).state, "queued");
  assert.equal(rig.runtime.snapshot().writerPending, false);
  assert.equal(
    rig.runtime.snapshot().activeOperations,
    2,
    "both queued jobs retain activity even when no writer is running",
  );
  assert.equal(rig.backend.scheduler.cancelRoot(rig.root), true);
  await bounded(
    rig.backend.scheduler.waitForRootIdle(rig.root),
    "cancelled retry and followup did not drain",
  );
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(rig.backend.scheduler.get(first.id).state, "cancelled");
  assert.equal(rig.backend.scheduler.get(followupId).state, "cancelled");
  assert.equal(rig.runtime.snapshot().activeOperations, 0);
  assert.equal(rig.calls.services, rig.calls.serviceCloses);
  assert.equal(rig.backend.modelPool.snapshot().activeLeases, 0);
});

async function fixture(t, options = {}) {
  const temporary = await mkdtemp(join(tmpdir(), "zvec-source-policy-"));
  const path = join(temporary, "repo");
  await mkdir(path);
  const root = await realpath(path);
  const source = join(root, "pool.ts");
  const provider = options.provider ?? "qwen";
  const name = provider === "qwen" ? "text-embedding-v4" : "potion-code-16m-v2";
  const endpoint =
    provider === "qwen" ? "https://unused.invalid/embeddings" : undefined;
  const store = new RemoteEmbeddingAuthorizationStore({
    signingKeyPath: join(temporary, "signing.key"),
  });
  const guard = remoteEmbeddingAuthorizationGuard({ store });
  const calls = {
    modelLoads: 0,
    authorizationChecks: 0,
    queryEmbeddings: 0,
    documentEmbeddings: 0,
    services: 0,
    serviceCloses: 0,
    indexInputs: [],
    fullScans: 0,
    sessionOpens: 0,
    sessionCloses: 0,
    submissions: [],
  };
  const hasGrant = store.hasGrant.bind(store);
  t.mock.method(store, "hasGrant", async (target) => {
    calls.authorizationChecks += 1;
    return hasGrant(target);
  });
  const state = { reportInvalidation: true };
  class FixtureModel extends FakeEmbeddingModel {
    info = {
      ...this.info,
      reference: `${provider}/${name}`,
      provider,
      name,
      endpoint,
    };
    async doEmbed(contents, embedOptions) {
      if (provider === "qwen") {
        await guard({
          provider,
          model: name,
          endpoint,
          purpose: embedOptions.purpose,
        });
      }
      if (embedOptions.purpose === "document") calls.documentEmbeddings += 1;
      else calls.queryEmbeddings += 1;
      return super.doEmbed(contents, embedOptions);
    }
  }
  const file = {
    id: "pool-file",
    absolutePath: source,
    relativePath: "pool.ts",
    rootPath: root,
    kind: "code",
    format: "typescript",
    sizeBytes: 64,
    lastModifiedTime: 1,
    contentHash: "indexed-hash",
  };
  const workspaceIndex = {
    id: "policy-workspace",
    name: "policy-workspace",
    path: join(root, ".zvec-grep"),
    rootPaths: [{ absolutePath: root, recursive: true }],
    embedding: { provider, model: name, dimension: 16, metric: "cosine" },
    indexVersion: CURRENT_INDEX_VERSION,
    createdTime: 1,
    updatedTime: 1,
  };
  const evidence = {
    indexed: {
      workspaceIndexId: workspaceIndex.id,
      fileId: file.id,
      absolutePath: source,
      rootPath: root,
      indexedTime: 1,
      contentHash: file.contentHash,
      sizeBytes: 64,
    },
    reason: "hash_mismatch",
    observedHash: "current-hash",
    observedSizeBytes: 64,
  };
  const freshStatus = {
    filesAdded: 0,
    filesModified: 0,
    filesDeleted: 0,
    filesPending: 0,
    filesFailed: 0,
    filesIndexed: 1,
    filesTotal: 1,
  };
  const info = {
    root,
    indexed: true,
    indexPolicy: "enabled",
    home: join(root, ".zvec-grep"),
    indexPath: workspaceIndex.path,
    source: "index",
    workspaceIndex,
  };
  const inspectRoot = async (requestedRoot, _serviceOptions, includeStatus) => {
    assert.equal(requestedRoot, root);
    if (includeStatus) calls.fullScans += 1;
    return { ...info, status: includeStatus ? freshStatus : undefined };
  };
  const storage = { listFiles: () => [file] };
  let watcherOptions;
  const backend = new DaemonBackend({
    version: "1.0.0",
    authorizationStore: store,
    serviceOptions: { endpoint, apiKey: "unused-fixture-key" },
    inspectRoot,
    schedulerOptions: options.schedulerOptions,
    modelPoolOptions: {
      createModel: (request) => {
        calls.modelLoads += 1;
        return options.createModel
          ? options.createModel(request, () => new FixtureModel())
          : new FixtureModel();
      },
    },
    watchManagerFactory: (watcher) => {
      watcherOptions = watcher;
      return {
        start() {},
        flushPending: async () => {},
        close: async () => {},
      };
    },
    logger: { event: (name, fields) => options.onEvent?.(name, fields) },
    createService: async () => {
      calls.services += 1;
      return {
        index: async (input) => {
          calls.indexInputs.push(input);
          if (options.index) return options.index(input);
          return {
            sourceFreshness: {
              workspaceIndexId: workspaceIndex.id,
              paths: [
                { absolutePath: join(root, "other.ts"), status: "absent" },
              ],
            },
          };
        },
        close: async () => {
          calls.serviceCloses += 1;
        },
      };
    },
  });
  const runtime = new RootRuntime({
    canonicalRoot: root,
    modelPool: backend.modelPool,
    openSession: async () => {
      calls.sessionOpens += 1;
      let closed = false;
      return {
        root,
        preflight: async (plan) => {
          assert.equal(closed, false);
          const searches = await Promise.all(
            plan.searches.map((search) =>
              preflightSearchPlan(search, { workspaceIndex, storage }),
            ),
          );
          return {
            plan,
            searches,
            workspaceIndex,
            embeddingEndpoint: endpoint,
            requiresEmbedding: searches.some(
              (search, index) =>
                search.hasSearchableFiles &&
                plan.searches[index].routes.some(
                  (route) => route.mode === "vector",
                ),
            ),
          };
        },
        contextPrepared: async (prepared) => {
          assert.equal(closed, false);
          return {
            query: prepared.plan.request.displayQuery,
            root,
            source: "index",
            coverage: "ranked_sample",
            items: state.reportInvalidation
              ? [
                  {
                    status: "possibly_stale",
                    content: "indexed source content",
                  },
                ]
              : [],
            diagnostics: {
              index: {
                sourceInvalidations: state.reportInvalidation ? [evidence] : [],
              },
            },
          };
        },
        close: async () => {
          assert.equal(closed, false);
          closed = true;
          calls.sessionCloses += 1;
        },
      };
    },
  });
  // Only root discovery and storage are mocked. Search preparation, evidence
  // recording, authorization, scheduling, coordination and job activity are real.
  t.mock.method(backend.runtimeManager, "activate", async () => runtime);
  t.mock.method(
    backend.runtimeManager,
    "activateForIndex",
    async () => runtime,
  );
  t.mock.method(backend.runtimeManager, "getByCanonicalRoot", () => runtime);
  const submit = backend.scheduler.submit.bind(backend.scheduler);
  t.mock.method(backend.scheduler, "submit", (input) => {
    const result = submit(input);
    calls.submissions.push(result);
    return result;
  });
  t.after(async () => {
    try {
      await bounded(backend.close(), "mock backend did not close");
    } finally {
      try {
        await bounded(runtime.close(), "mock runtime did not close");
        assert.equal(calls.sessionOpens, calls.sessionCloses);
      } finally {
        await rm(temporary, { recursive: true, force: true });
      }
    }
  });
  return {
    root,
    source,
    store,
    backend,
    runtime,
    calls,
    state,
    inspectRoot,
    watcher: () => watcherOptions,
    queryOnlyPermit: async () => {
      const plan = await planRemoteSearchAuthorization({
        info: { ...info, status: freshStatus },
        model: new FixtureModel().info,
        search: ordinaryInput(root),
        store,
      });
      assert.equal(plan.operation, "query");
      assert.deepEqual(plan.disclosure, {
        queryText: true,
        workspaceContent: "none",
      });
      return backend.grantRemoteEmbedding(plan, "once");
    },
  };
}

function assertNoJob(rig) {
  assert.equal(rig.backend.scheduler.getByRoot(rig.root), undefined);
  assert.deepEqual(rig.backend.scheduler.snapshot(), { queued: 0, running: 0 });
  assert.equal(rig.calls.submissions.length, 0);
}

function ordinaryInput(root) {
  return {
    root,
    queries: ["connectionPool"],
    routes: [],
    freshness: "eventual",
    autoUpdate: true,
  };
}

function ftsInput(root) {
  return {
    root,
    queries: [],
    routes: [{ mode: "fts", query: "connectionPool" }],
    freshness: "eventual",
    autoUpdate: false,
  };
}

function changes(path) {
  return {
    touchedFiles: [path],
    rescanDirectories: [],
    deletedPrefixes: [],
    forceFullReconcile: false,
  };
}

function deferred() {
  let resolve;
  const promise = new Promise((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

async function bounded(promise, message) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(message)), 3_000);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}
