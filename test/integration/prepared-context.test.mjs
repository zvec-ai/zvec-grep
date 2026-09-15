import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { createZvecGrep } from "../../dist/index.js";
import {
  openWorkspaceReadSession,
  planWorkspaceContext,
  prepareWorkspaceContext,
} from "../../dist/engine/service/zvec-grep.js";
import {
  readWorkspaceManifest,
  writeWorkspaceManifest,
} from "../../dist/engine/manifest.js";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

class RecordingModel extends FakeEmbeddingModel {
  queries = [];

  async doEmbed(contents, options) {
    if (options?.purpose === "query") {
      this.queries.push(contents.map((content) => content.text));
    }
    return super.doEmbed(contents, options);
  }
}

async function fixture(t) {
  const root = await createTemporaryDirectory(t, "zvec-prepared-context-");
  await writeFile(
    join(root, "pool.ts"),
    "export function connectionPool() { return 'reuse connections'; }\n",
  );
  await writeFile(
    join(root, "retry.ts"),
    "export function requestTimeout() { return 'retry requests'; }\n",
  );
  const model = new RecordingModel();
  const service = await createZvecGrep({ root, embeddingModel: model });
  t.after(() => service.close());
  await service.index();
  return { root, model, service };
}

function resultShape(result) {
  return {
    query: result.query,
    groups: result.groupResults,
    items: result.items,
    routes: result.diagnostics.index.routes,
  };
}

test("a model-free read session executes prepared FTS groups and rejects unprepared vectors", async (t) => {
  const { root, model } = await fixture(t);
  const session = openWorkspaceReadSession(root);
  try {
    const plan = planWorkspaceContext({
      routes: [
        { mode: "fts", query: "connectionPool" },
        { mode: "fts", query: "requestTimeout" },
      ],
    });
    const preflight = await session.preflight(plan);
    assert.equal(preflight.requiresEmbedding, false);
    const prepared = await prepareWorkspaceContext(plan, undefined, preflight);
    const result = await session.contextPrepared(prepared);
    assert.equal(result.groupResults.length, 2);
    assert.ok(result.groupResults.every((group) => group.items.length > 0));
    assert.deepEqual(model.queries, []);
    const vectorPlan = planWorkspaceContext({
      routes: [{ mode: "vector", query: "connection pool" }],
    });
    await assert.rejects(
      prepareWorkspaceContext(
        vectorPlan,
        undefined,
        await session.preflight(vectorPlan),
      ),
      { code: "ZVEC_GREP.ENGINE.SEARCH.EMBEDDING_MODEL_REQUIRED" },
    );
  } finally {
    await session.close();
  }
  await assert.rejects(
    session.preflight(planWorkspaceContext({ query: "closed" })),
    {
      code: "ZVEC_GREP.ENGINE.SERVICE.READ_SESSION_CLOSED",
    },
  );
});

test("prepared independent and fused groups retain existing routes, ranking, and query identity", async (t) => {
  const { root, model, service } = await fixture(t);
  for (const fuse of [false, true]) {
    const options = {
      queries: ["connection pool", "request timeout"],
      routes: [
        { mode: "fts", query: "connectionPool" },
        { mode: "vector", query: "retry requests" },
      ],
      autoUpdate: false,
      fuse,
    };
    const baseline = await service.context(options);
    model.queries = [];
    const session = openWorkspaceReadSession(root);
    try {
      const plan = planWorkspaceContext(options);
      const prepared = await prepareWorkspaceContext(
        plan,
        model,
        await session.preflight(plan),
      );
      assert.deepEqual(model.queries.flat(), [
        "connection pool",
        "request timeout",
        "retry requests",
      ]);
      const queryCalls = model.queries.length;
      const result = await session.contextPrepared(prepared);
      assert.equal(
        model.queries.length,
        queryCalls,
        "consumption never embeds again",
      );
      assert.deepEqual(resultShape(result), resultShape(baseline));
      if (!fuse) {
        assert.deepEqual(
          prepared.searches.map((search) => [...search.vectorsByRoute.keys()]),
          [["vector"], ["vector"], [], ["vector"]],
        );
      }
    } finally {
      await session.close();
    }
  }
});

test("empty-filter preparation stays model-free and detects files added before consumption", async (t) => {
  const { root, model, service } = await fixture(t);
  const plan = planWorkspaceContext({
    query: "connection pool",
    includePaths: ["new/**"],
  });
  let session = openWorkspaceReadSession(root);
  let prepared;
  try {
    const preflight = await session.preflight(plan);
    assert.equal(preflight.requiresEmbedding, false);
    prepared = await prepareWorkspaceContext(plan, undefined, preflight);
    assert.deepEqual((await session.contextPrepared(prepared)).items, []);
    assert.deepEqual(model.queries, []);
  } finally {
    await session.close();
  }
  await mkdir(join(root, "new"));
  await writeFile(
    join(root, "new", "pool.ts"),
    "export function connectionPool() { return 'new connections'; }\n",
  );
  await service.index();
  session = openWorkspaceReadSession(root);
  try {
    await assert.rejects(session.contextPrepared(prepared), {
      code: "ZVEC_GREP.ENGINE.SEARCH.PREPARED_VECTORS_REQUIRED",
    });
    const preflight = await session.preflight(plan);
    assert.equal(preflight.requiresEmbedding, true);
    const ready = await prepareWorkspaceContext(plan, model, preflight);
    const result = await session.contextPrepared(ready);
    assert.ok(result.items.length > 0);
    assert.ok(
      result.items.every((item) => item.file.relativePath === "new/pool.ts"),
    );
    assert.deepEqual(model.queries, [["connection pool"]]);
  } finally {
    await session.close();
  }
});

test("prepared context rejects changed schemas and endpoints instead of embedding against a new destination", async (t) => {
  const { root, model, service } = await fixture(t);
  const plan = planWorkspaceContext({ query: "connection pool" });
  let session = openWorkspaceReadSession(root);
  let prepared;
  try {
    const preflight = await session.preflight(plan);
    const mismatched = new RecordingModel();
    mismatched.info = { ...mismatched.info, name: "different-model" };
    await assert.rejects(prepareWorkspaceContext(plan, mismatched, preflight), {
      code: "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.EMBEDDING_MODEL_MISMATCH",
    });
    assert.deepEqual(mismatched.queries, []);
    prepared = await prepareWorkspaceContext(plan, model, preflight);
  } finally {
    await session.close();
  }
  const { home } = await service.info({ includeStatus: false });
  const manifest = readWorkspaceManifest(home);
  for (const [changed, code] of [
    [
      {
        ...manifest,
        embedding: { ...manifest.embedding, model: "different-model" },
      },
      "ZVEC_GREP.ENGINE.WORKSPACE_INDEX.EMBEDDING_MODEL_MISMATCH",
    ],
    [
      {
        ...manifest,
        embeddingRuntime: {
          ...manifest.embeddingRuntime,
          endpoint: "https://example.invalid/embedding",
        },
      },
      "ZVEC_GREP.ENGINE.SERVICE.SEARCH_ENDPOINT_CHANGE_REQUIRES_REBUILD",
    ],
  ]) {
    writeWorkspaceManifest(home, changed);
    session = openWorkspaceReadSession(root);
    try {
      await assert.rejects(session.contextPrepared(prepared), { code });
    } finally {
      await session.close();
    }
  }
  assert.deepEqual(model.queries, [["connection pool"]]);
});

test("all context groups and filters are validated before any query preparation", () => {
  assert.throws(
    () =>
      planWorkspaceContext({
        query: "connection pool",
        routes: [{ mode: "vector", query: "" }],
      }),
    { code: "ZVEC_GREP.ENGINE.SERVICE.EMPTY_ROUTE_QUERY" },
  );
  assert.throws(
    () =>
      planWorkspaceContext({
        queries: ["connection pool", "request timeout"],
        modifiedAfter: 200,
        modifiedBefore: 100,
      }),
    { code: "ZVEC_GREP.ENGINE.SEARCH_PLAN.INVALID_MODIFIED_TIME_RANGE" },
  );
});

test("caller option mutation during embedding cannot change prepared groups, filters, or results", async (t) => {
  const { root, service } = await fixture(t);
  const options = {
    queries: ["connection pool"],
    routes: [
      { mode: "vector", query: "request timeout" },
      { mode: "fts", query: "connectionPool" },
    ],
    autoUpdate: false,
    symbolTypes: ["function"],
    includePaths: ["*.ts"],
    excludePaths: ["missing/**"],
    globs: ["*.ts"],
    insensitiveGlobs: ["*.TS"],
    fileTypes: ["ts"],
    excludedFileTypes: ["go"],
    rgPaths: ["."],
    ignoreFiles: [".gitignore"],
    rgOptions: { extraArgs: ["--line-number"], patternFiles: ["patterns.txt"] },
  };
  const original = structuredClone(options);
  const baseline = await service.context(options);
  const session = openWorkspaceReadSession(root);
  const entered = Promise.withResolvers();
  const released = Promise.withResolvers();
  class HeldModel extends RecordingModel {
    async doEmbed(contents, embedOptions) {
      entered.resolve();
      await released.promise;
      return super.doEmbed(contents, embedOptions);
    }
  }
  const model = new HeldModel();
  try {
    const plan = planWorkspaceContext(options);
    const preparation = prepareWorkspaceContext(
      plan,
      model,
      await session.preflight(plan),
    );
    try {
      await entered.promise;
      for (const [field, value] of Object.entries(options)) {
        if (!Array.isArray(value)) continue;
        if (field === "routes") {
          value[0].query = "caller changed the query";
          value.push({ mode: "vector", query: "caller added a route" });
        } else {
          value.splice(0, value.length, "caller changed the option");
        }
      }
      options.rgOptions.extraArgs.push("--changed");
      options.rgOptions.patternFiles[0] = "changed-patterns.txt";
      options.fuse = true;
      options.limit = 1;
    } finally {
      released.resolve();
    }
    const prepared = await preparation;
    assert.deepEqual(
      JSON.parse(JSON.stringify(prepared.plan.options)),
      original,
    );
    assert.deepEqual(model.queries.flat(), [
      "connection pool",
      "request timeout",
    ]);
    const result = await session.contextPrepared(prepared);
    assert.deepEqual(resultShape(result), resultShape(baseline));
    assert.ok(result.items.length > 0);
    assert.ok(
      result.items.every((item) => item.metadata.symbolType === "function"),
    );
  } finally {
    released.resolve();
    await session.close();
    await model.dispose();
  }
});

test("context cancellation preserves its reason, stops later groups, and leaves independent FTS usable", async (t) => {
  const { root } = await fixture(t);
  const session = openWorkspaceReadSession(root);
  const controller = new AbortController();
  const reason = new Error("cancel the semantic request");
  const entered = Promise.withResolvers();
  const released = Promise.withResolvers();
  const calls = [];
  class Model extends RecordingModel {
    async doEmbed(contents, options) {
      calls.push(contents.map((content) => content.text));
      assert.equal(options.signal, controller.signal);
      entered.resolve();
      await released.promise;
      return super.doEmbed(contents, options);
    }
  }
  const model = new Model();
  try {
    const plan = planWorkspaceContext(
      { queries: ["connection pool", "request timeout"] },
      controller.signal,
    );
    const preparation = prepareWorkspaceContext(
      plan,
      model,
      await session.preflight(plan),
      controller.signal,
    );
    try {
      await entered.promise;
      controller.abort(reason);
      const local = planWorkspaceContext({
        routes: [{ mode: "fts", query: "connectionPool" }],
      });
      const ready = await prepareWorkspaceContext(
        local,
        undefined,
        await session.preflight(local),
      );
      assert.ok((await session.contextPrepared(ready)).items.length > 0);
    } finally {
      released.resolve();
    }
    await assert.rejects(preparation, (error) => error === reason);
    assert.deepEqual(calls, [["connection pool"]]);
    assert.throws(
      () =>
        planWorkspaceContext({ query: "already cancelled" }, controller.signal),
      (error) => error === reason,
    );
    const local = planWorkspaceContext({
      routes: [{ mode: "fts", query: "connectionPool" }],
    });
    await assert.rejects(
      prepareWorkspaceContext(
        local,
        undefined,
        await session.preflight(local),
        controller.signal,
      ),
      (error) => error === reason,
    );
  } finally {
    released.resolve();
    await session.close();
    await model.dispose();
  }
});

test("intentional local-only execution preserves group intent, fusion, and filters without a model", async (t) => {
  const { root, model } = await fixture(t);
  const session = openWorkspaceReadSession(root);
  try {
    for (const fuse of [false, true]) {
      const options = {
        queries: ["connectionPool", "requestTimeout"],
        routes: [{ mode: "fts", query: "connectionPool" }],
        fuse,
        globs: ["*.ts"],
        excludePaths: ["retry.ts"],
        symbolTypes: ["function"],
        modifiedAfter: 0,
        autoUpdate: false,
      };
      const full = planWorkspaceContext(options);
      const local = planWorkspaceContext(options, undefined, "fts_only");
      assert.equal(full.execution, "all");
      assert.equal(local.execution, "fts_only");
      assert.deepEqual(local.options, full.options);
      assert.deepEqual(local.request, full.request);
      assert.deepEqual(
        local.groups.map(({ id, query, role }) => ({ id, query, role })),
        full.groups.map(({ id, query, role }) => ({ id, query, role })),
      );
      assert.ok(
        local.groups.every((group) =>
          group.routes.every((route) => route.mode === "fts"),
        ),
      );
      assert.ok(
        full.groups.some((group) =>
          group.routes.some((route) => route.mode === "vector"),
        ),
      );
      const preflight = await session.preflight(local);
      assert.equal(preflight.requiresEmbedding, false);
      const prepared = await prepareWorkspaceContext(
        local,
        undefined,
        preflight,
      );
      const result = await session.contextPrepared(prepared);
      assert.equal(result.query, full.request.displayQuery);
      assert.equal(result.groupResults.length, full.groups.length);
      assert.ok(result.items.length > 0);
      assert.ok(
        result.items.every((item) => item.file.relativePath === "pool.ts"),
      );
      assert.ok(
        result.diagnostics.index.routes.every((route) => route.mode === "fts"),
      );
      for (const [index, group] of local.groups.entries()) {
        const baseline = await session.context({
          ...options,
          query: undefined,
          queries: undefined,
          routes: group.routes,
          fuse: true,
        });
        assert.deepEqual(
          result.groupResults[index].items.map((item) => item.entityId),
          baseline.groupResults[0].items.map((item) => item.entityId),
        );
      }
    }
    assert.deepEqual(model.queries, []);
  } finally {
    await session.close();
  }
});

test("local-only plans reject explicit vectors and still validate the complete original request", () => {
  for (const fuse of [false, true]) {
    for (const queries of [undefined, ["connection pool"]]) {
      assert.throws(
        () =>
          planWorkspaceContext(
            {
              queries,
              routes: [{ mode: "vector", query: "request timeout" }],
              fuse,
            },
            undefined,
            "fts_only",
          ),
        {
          code: "ZVEC_GREP.ENGINE.SEARCH.LOCAL_FALLBACK_EXPLICIT_VECTOR",
        },
      );
    }
  }
  assert.throws(
    () =>
      planWorkspaceContext(
        { query: "connection pool", modifiedAfter: 200, modifiedBefore: 100 },
        undefined,
        "fts_only",
      ),
    {
      code: "ZVEC_GREP.ENGINE.SEARCH_PLAN.INVALID_MODIFIED_TIME_RANGE",
    },
  );
  assert.throws(
    () =>
      planWorkspaceContext(
        { query: "connection pool", routes: [{ mode: "vector", query: "" }] },
        undefined,
        "fts_only",
      ),
    {
      code: "ZVEC_GREP.ENGINE.SERVICE.EMPTY_ROUTE_QUERY",
    },
  );
});
