import assert from "node:assert/strict";
import test from "node:test";
import {
  preflightSearchPlan,
  prepareSearchPlan,
  searchWorkspaceIndex,
} from "../../dist/engine/pipeline/search/index.js";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";

function fixture() {
  const files = [];
  const calls = { fts: 0, vector: 0 };
  const metadata = { lists: 0 };
  const filters = [];
  return {
    files,
    calls,
    metadata,
    filters,
    context: {
      workspaceIndex: { name: "prepared-search" },
      storage: {
        listFiles: () => {
          metadata.lists += 1;
          return files;
        },
        searchFts: (_query, _limit, filter) => {
          calls.fts += 1;
          filters.push(filter);
          return [];
        },
        searchVector: () => {
          calls.vector += 1;
          return [];
        },
      },
    },
  };
}

test("prepared FTS executes without a model and without vector work", async () => {
  const { calls, context } = fixture();
  const plan = { routes: [{ mode: "fts", query: "connection pool" }] };
  const preflight = await preflightSearchPlan(plan, context);
  const prepared = await prepareSearchPlan(plan, undefined, preflight);
  const result = await searchWorkspaceIndex(plan, context, prepared);
  assert.deepEqual(result.hits, []);
  assert.ok(calls.fts > 0);
  assert.equal(calls.vector, 0);
  assert.equal(prepared.embedding, undefined);
  assert.equal(prepared.vectorsByRoute.size, 0);
});

test("unfiltered prepared searches never enumerate the file table, including symbol-only filters", async () => {
  for (const modes of [["fts"], ["vector"], ["fts", "vector"]]) {
    for (const symbolTypes of [undefined, ["function"]]) {
      const { context, metadata, filters } = fixture();
      const plan = {
        routes: modes.map((mode) => ({ mode, query: "connection pool" })),
        symbolTypes,
      };
      const preflight = await preflightSearchPlan(plan, context);
      assert.equal(metadata.lists, 0);
      const prepared = await prepareSearchPlan(
        plan,
        new FakeEmbeddingModel(),
        preflight,
      );
      await searchWorkspaceIndex(plan, context, prepared);
      assert.equal(metadata.lists, 0);
      assert.ok(filters.every((filter) => filter?.fileIds === undefined));
      if (symbolTypes) {
        assert.ok(
          filters.every((filter) => filter.symbolTypes.includes("function")),
        );
      }
    }
  }
});

test("filtered FTS defers its only metadata enumeration to consumption", async () => {
  const { context, files, metadata, filters } = fixture();
  const plan = {
    routes: [{ mode: "fts", query: "connection pool" }],
    includePaths: ["src/**"],
  };
  const preflight = await preflightSearchPlan(plan, context);
  assert.equal(metadata.lists, 0);
  const prepared = await prepareSearchPlan(plan, undefined, preflight);
  files.push({
    id: "new-file",
    absolutePath: "/repo/src/pool.ts",
    relativePath: "src/pool.ts",
    rootPath: "/repo",
  });
  await searchWorkspaceIndex(plan, context, prepared);
  assert.equal(metadata.lists, 1);
  assert.ok(filters.length > 0);
  assert.ok(filters.every((filter) => filter.fileIds.join() === "new-file"));
});

test("every file-filter family still enumerates and rechecks metadata for vector preparation", async () => {
  const cases = [
    [{ includePaths: ["src/**"] }, ["ts", "go"]],
    [{ excludePaths: ["**/*.ts"] }, ["go"]],
    [{ globs: ["*.ts"] }, ["ts", "test"]],
    [{ insensitiveGlobs: ["*.TS"] }, ["ts", "test"]],
    [{ fileTypes: ["ts"] }, ["ts", "test"]],
    [{ excludedFileTypes: ["ts"] }, ["go"]],
    [{ modifiedAfter: 150 }, ["go", "test"]],
    [{ modifiedBefore: 150 }, ["ts"]],
  ];
  for (const [selection, expected] of cases) {
    const { context, files, metadata, filters } = fixture();
    for (const [id, relativePath, lastModifiedTime] of [
      ["ts", "src/main.ts", 100],
      ["go", "src/main.go", 200],
      ["test", "test/main.ts", 300],
    ]) {
      files.push({
        id,
        relativePath,
        lastModifiedTime,
        rootPath: "/repo",
        absolutePath: `/repo/${relativePath}`,
      });
    }
    const plan = {
      routes: [
        { mode: "fts", query: "connection pool" },
        { mode: "vector", query: "connection pool" },
      ],
      ...selection,
    };
    const preflight = await preflightSearchPlan(plan, context);
    assert.equal(preflight.hasSearchableFiles, true);
    assert.equal(metadata.lists, 1);
    const prepared = await prepareSearchPlan(
      plan,
      new FakeEmbeddingModel(),
      preflight,
    );
    await searchWorkspaceIndex(plan, context, prepared);
    assert.equal(metadata.lists, 2, JSON.stringify(selection));
    assert.ok(filters.length > 0);
    for (const filter of filters) assert.deepEqual(filter.fileIds, expected);
  }
});

test("an empty path preflight skips embedding, but newly matching files require preparation", async () => {
  const { calls, context, files } = fixture();
  const plan = {
    routes: [
      { mode: "fts", query: "connection pool" },
      { mode: "vector", query: "connection pool" },
    ],
    includePaths: ["src/**"],
  };
  const preflight = await preflightSearchPlan(plan, context);
  assert.equal(preflight.hasSearchableFiles, false);
  const prepared = await prepareSearchPlan(plan, undefined, preflight);
  const empty = await searchWorkspaceIndex(plan, context, prepared);
  assert.deepEqual(empty.hits, []);
  assert.deepEqual(calls, { fts: 0, vector: 0 });

  files.push({
    id: "pool",
    absolutePath: "/repo/src/pool.ts",
    relativePath: "src/pool.ts",
    rootPath: "/repo",
  });
  await assert.rejects(searchWorkspaceIndex(plan, context, prepared), {
    code: "ZVEC_GREP.ENGINE.SEARCH.PREPARED_VECTORS_REQUIRED",
  });
  assert.deepEqual(calls, { fts: 0, vector: 0 });
  const ready = await prepareSearchPlan(
    plan,
    new FakeEmbeddingModel(),
    await preflightSearchPlan(plan, context),
  );
  await searchWorkspaceIndex(plan, context, ready);
  assert.ok(calls.fts > 0);
  assert.ok(calls.vector > 0);
});

test("prepared vectors remain bound to every normalized route and query", async () => {
  const { context } = fixture();
  const model = new FakeEmbeddingModel();
  const plan = {
    routes: [
      { mode: "vector", query: "connection pool" },
      { mode: "vector", query: "request timeout" },
      { mode: "fts", query: "retry" },
    ],
  };
  const prepared = await prepareSearchPlan(plan, model);
  assert.deepEqual([...prepared.vectorsByRoute.keys()], ["vector", "vector-2"]);
  assert.notDeepEqual(
    prepared.vectorsByRoute.get("vector"),
    prepared.vectorsByRoute.get("vector-2"),
  );
  await searchWorkspaceIndex(plan, context, prepared);
  await assert.rejects(
    searchWorkspaceIndex(
      { ...plan, routes: [...plan.routes].reverse() },
      context,
      prepared,
    ),
    { code: "ZVEC_GREP.ENGINE.SEARCH.PREPARED_PLAN_MISMATCH" },
  );
  await assert.rejects(
    searchWorkspaceIndex({ ...plan, limit: 1 }, context, prepared),
    { code: "ZVEC_GREP.ENGINE.SEARCH.PREPARED_PLAN_MISMATCH" },
  );
});

test("prepared consumption never silently embeds a missing or malformed vector", async () => {
  const { context } = fixture();
  let calls = 0;
  context.embeddingModel = {
    embed() {
      calls += 1;
      throw new Error("consume must not embed");
    },
  };
  const plan = { routes: [{ mode: "vector", query: "connection pool" }] };
  const prepared = await prepareSearchPlan(plan, new FakeEmbeddingModel());
  for (const vector of [[1], Array(16).fill(Number.NaN)]) {
    await assert.rejects(
      searchWorkspaceIndex(plan, context, {
        ...prepared,
        vectorsByRoute: new Map([["vector", vector]]),
      }),
      { code: "ZVEC_GREP.ENGINE.SEARCH.INVALID_PREPARED_VECTOR" },
    );
  }
  await assert.rejects(
    searchWorkspaceIndex(plan, context, {
      ...prepared,
      vectorsByRoute: new Map(),
    }),
    { code: "ZVEC_GREP.ENGINE.SEARCH.PREPARED_VECTORS_REQUIRED" },
  );
  assert.equal(calls, 0);
});

test("query preparation snapshots caller routes and symbol filters across embedding waits", async () => {
  const { context, filters } = fixture();
  const entered = Promise.withResolvers();
  const released = Promise.withResolvers();
  class HeldModel extends FakeEmbeddingModel {
    async doEmbed(contents, options) {
      entered.resolve();
      await released.promise;
      return super.doEmbed(contents, options);
    }
  }
  const plan = {
    routes: [
      { mode: "fts", query: "connection pool" },
      { mode: "vector", query: "connection pool" },
    ],
    symbolTypes: ["function"],
  };
  const preparation = prepareSearchPlan(plan, new HeldModel());
  try {
    await entered.promise;
    plan.symbolTypes[0] = "class";
    plan.routes[0].query = "changed fts query";
    plan.routes[1].query = "changed vector query";
  } finally {
    released.resolve();
  }
  const prepared = await preparation;
  assert.deepEqual(prepared.plan.symbolTypes, ["function"]);
  assert.deepEqual(
    prepared.plan.routes.map((route) => route.query),
    ["connection pool", "connection pool"],
  );
  await searchWorkspaceIndex(prepared.plan, context, prepared);
  assert.ok(filters.length > 0);
  assert.ok(
    filters.every((filter) => filter.symbolTypes.join() === "function"),
  );
});

test("pre-cancelled preparation preserves the abort reason without submitting vector or FTS work", async () => {
  for (const reason of [
    undefined,
    new Error("caller cancelled"),
    "caller reason",
  ]) {
    const controller = new AbortController();
    controller.abort(reason);
    let calls = 0;
    class Model extends FakeEmbeddingModel {
      async doEmbed(contents, options) {
        calls += 1;
        return super.doEmbed(contents, options);
      }
    }
    for (const [mode, hasSearchableFiles] of [
      ["fts", true],
      ["vector", true],
      ["vector", false],
    ]) {
      await assert.rejects(
        prepareSearchPlan(
          { routes: [{ mode, query: "connection pool" }] },
          new Model(),
          { hasSearchableFiles },
          controller.signal,
        ),
        (error) => error === controller.signal.reason,
      );
    }
    assert.equal(calls, 0);
  }
});

test("in-flight preparation passes the signal through BaseEmbeddingModel and unwraps cancelled backend failures", async () => {
  const controller = new AbortController();
  const reason = new Error("stop this query");
  const entered = Promise.withResolvers();
  const calls = [];
  class Model extends FakeEmbeddingModel {
    info = { ...this.info, limits: { maxBatchSize: 1 } };
    async doEmbed(contents, options) {
      calls.push(contents.map((content) => content.text));
      assert.equal(options.signal, controller.signal);
      assert.equal(options.purpose, "query");
      entered.resolve();
      await new Promise((resolve) =>
        options.signal.addEventListener("abort", resolve, { once: true }),
      );
      throw new Error("backend wrapped its cancelled operation");
    }
  }
  const preparation = prepareSearchPlan(
    {
      routes: [
        { mode: "vector", query: "first query" },
        { mode: "vector", query: "second query" },
      ],
    },
    new Model(),
    undefined,
    controller.signal,
  );
  await entered.promise;
  controller.abort(reason);
  await assert.rejects(preparation, (error) => error === reason);
  assert.deepEqual(calls, [["first query"]]);
});

test("a backend ignoring abort cannot publish a late prepared result or start another batch", async () => {
  const controller = new AbortController();
  const reason = new Error("ignore late vectors");
  const entered = Promise.withResolvers();
  const released = Promise.withResolvers();
  const calls = [];
  class Model extends FakeEmbeddingModel {
    info = { ...this.info, limits: { maxBatchSize: 1 } };
    async doEmbed(contents, options) {
      calls.push(contents.map((content) => content.text));
      assert.equal(options.signal, controller.signal);
      entered.resolve();
      await released.promise;
      return super.doEmbed(contents, options);
    }
  }
  let settled = false;
  const preparation = prepareSearchPlan(
    {
      routes: [
        { mode: "vector", query: "first query" },
        { mode: "vector", query: "second query" },
      ],
    },
    new Model(),
    undefined,
    controller.signal,
  );
  void preparation.then(
    () => {
      settled = true;
    },
    () => {
      settled = true;
    },
  );
  try {
    await entered.promise;
    controller.abort(reason);
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(
      settled,
      false,
      "propagation does not claim to terminate ignored native work",
    );
  } finally {
    released.resolve();
  }
  await assert.rejects(preparation, (error) => error === reason);
  assert.deepEqual(calls, [["first query"]]);
});

test("ordinary embedding failures retain their original identity when the signal is not cancelled", async () => {
  const failure = new Error("model really failed");
  class Model extends FakeEmbeddingModel {
    async doEmbed() {
      throw failure;
    }
  }
  const controller = new AbortController();
  await assert.rejects(
    prepareSearchPlan(
      { routes: [{ mode: "vector", query: "connection pool" }] },
      new Model(),
      undefined,
      controller.signal,
    ),
    (error) => error === failure,
  );
  assert.equal(controller.signal.aborted, false);
});
