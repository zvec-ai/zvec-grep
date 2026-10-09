import assert from "node:assert/strict";
import test from "node:test";
import {
  diagnoseEntitySearch,
  diagnoseFileSearch,
  searchWorkspaceIndex,
} from "../../dist/engine/pipeline/search/index.js";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";
import { inSearchWorker } from "../helpers/search-worker.mjs";

function file(id, relativePath, lastModifiedTime = 100) {
  return {
    id,
    absolutePath: `/repo/${relativePath}`,
    relativePath,
    rootPath: "/repo",
    sizeBytes: 10,
    lastModifiedTime,
    kind: "code",
    format: "typescript",
  };
}

function entity(id, fileId, symbolName) {
  return {
    id,
    fileId,
    range: {
      kind: "text",
      startLine: 1,
      endLine: 3,
      startOffset: 0,
      endOffset: 30,
    },
    content: { kind: "text", text: `export function ${symbolName}() {}` },
    metadata: {
      kind: "code",
      symbolType: "function",
      symbolName,
      scope: null,
      nodeType: "function_declaration",
      signature: `function ${symbolName}()`,
      doc: null,
      modifiers: ["exported"],
    },
  };
}

function fragment(storedEntity, options = {}) {
  return {
    id: options.id ?? storedEntity.id,
    group: options.group,
    fileId: storedEntity.fileId,
    range: storedEntity.range,
    content: storedEntity.content,
    metadata: storedEntity.metadata,
  };
}

function createFixture() {
  const files = [
    file("file-a", "src/a.ts", 100),
    file("file-b", "src/b.test.ts", 200),
    file("file-c", "docs/c.ts", 300),
  ];
  const entities = [
    entity("entity-a", "file-a", "AlphaSymbol"),
    entity("entity-b", "file-b", "BetaSymbol"),
    entity("entity-c", "file-c", "GammaSymbol"),
  ];
  const calls = { fts: [], vector: [] };

  function stored(id) {
    const found = entities.find((item) => item.id === id);
    if (!found) return null;
    return {
      entity: found,
      file: files.find((item) => item.id === found.fileId),
      vector: [1, 0],
    };
  }

  function hits(path, filter) {
    return entities
      .filter(
        (item) => !filter?.fileIds || filter.fileIds.includes(item.fileId),
      )
      .filter((item) => !filter?.groupIds || filter.groupIds.includes(item.id))
      .filter(
        (item) =>
          !filter?.symbolNames ||
          filter.symbolNames.includes(item.metadata.symbolName),
      )
      .filter(
        (item) =>
          !filter?.symbolTypes ||
          filter.symbolTypes.includes(item.metadata.symbolType),
      )
      .map((item, index) => ({
        fragment:
          item.id === "entity-a" && path === "vector"
            ? fragment(item, { id: "entity-a-fragment", group: "entity-a" })
            : fragment(item),
        file: files.find((candidate) => candidate.id === item.fileId),
        path,
        score: 1 - index * 0.1,
      }));
  }

  const storage = {
    getFileById: (id) => files.find((item) => item.id === id) ?? null,
    getFileByPath: (path) =>
      files.find((item) => item.absolutePath === path) ?? null,
    listFiles: () => files,
    listEntitiesByFile: (fileId, options = {}) =>
      entities
        .filter((item) => item.fileId === fileId)
        .slice(
          options.offset ?? 0,
          (options.offset ?? 0) + (options.limit ?? 99),
        )
        .map((item) => ({
          entity: item,
          file: files.find((candidate) => candidate.id === item.fileId),
        })),
    getEntity: (id) => stored(id),
    upsertFile: () => {},
    markFileFailed: () => {},
    deleteFile: () => {},
    searchFts: (query, limit, filter) => {
      calls.fts.push({ query, limit, filter });
      return hits("fts", filter).slice(0, limit);
    },
    searchVector: (queryVector, limit, filter) => {
      calls.vector.push({ queryVector, limit, filter });
      return hits("vector", filter).reverse().slice(0, limit);
    },
    optimize: () => {},
    close: () => {},
  };
  return {
    files,
    entities,
    calls,
    storage,
    context: {
      workspaceIndex: {
        id: "workspace-index-1",
        name: "docs",
        path: "/tmp/index",
        rootPaths: [{ absolutePath: "/repo", recursive: true }],
        createdTime: 1,
        updatedTime: 1,
      },
      storage,
      embeddingModel: new FakeEmbeddingModel(),
    },
  };
}

test("search plan rejects malformed routes, filters, time ranges, and missing models", async () => {
  const { context } = createFixture();
  await assert.rejects(searchWorkspaceIndex({ routes: [] }, context), /route/);
  await assert.rejects(
    searchWorkspaceIndex(
      { routes: [{ mode: "unsupported", query: "value" }] },
      context,
    ),
    /unsupported mode/,
  );
  await assert.rejects(
    searchWorkspaceIndex({ routes: [{ mode: "fts", query: " " }] }, context),
    /non-empty query/,
  );
  await assert.rejects(
    searchWorkspaceIndex(
      { routes: [{ mode: "fts", query: "value" }], includePaths: "src" },
      context,
    ),
    /must be arrays/,
  );
  await assert.rejects(
    searchWorkspaceIndex(
      { routes: [{ mode: "fts", query: "value" }], excludePaths: [1] },
      context,
    ),
    /contain strings/,
  );
  await assert.rejects(
    searchWorkspaceIndex(
      { routes: [{ mode: "fts", query: "value" }], modifiedAfter: -1 },
      context,
    ),
    /non-negative/,
  );
  await assert.rejects(
    searchWorkspaceIndex(
      {
        routes: [{ mode: "fts", query: "value" }],
        modifiedAfter: 20,
        modifiedBefore: 10,
      },
      context,
    ),
    /must not be later/,
  );
  await assert.rejects(
    searchWorkspaceIndex(
      { routes: [{ mode: "vector", query: "value" }] },
      { ...context, embeddingModel: undefined },
    ),
    /requires an embedding model/,
  );
});

test("hybrid search filters, deduplicates, fuses, traces, prefers symbols, and tracks hidden hits", async () => {
  const fixture = createFixture();
  const result = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query: "find Namespace::AlphaSymbol" },
        { mode: "fts", query: "secondary" },
        { mode: "vector", query: "semantic alpha" },
        { mode: "vector", query: "semantic beta" },
      ],
      globs: ["src/**", "!**/*.test.ts"],
      fileTypes: ["ts"],
      modifiedAfter: 50,
      modifiedBefore: 250,
      symbolTypes: ["function"],
      preferSymbol: true,
      trace: true,
      limit: 1,
      trackEntityId: "entity-c",
    },
    fixture.context,
  );
  assert.equal(
    result.plan.routes.map((route) => route.id).join(","),
    "fts,fts-2,vector,vector-2",
  );
  assert.equal(result.hits[0].entity.id, "entity-a");
  assert.equal(result.hits[0].matchedBy, "fts+vector");
  assert.ok(result.hits[0].evidence.length >= 2);
  assert.equal(result.hits[0].trace.final.returnedByLimit, true);
  assert.equal(result.trackedHit?.entity.id, "entity-c");
  assert.equal(result.trackedHit?.trace.final.returnedByLimit, false);
  assert.ok(
    result.trackedHit?.trace.recall.every(
      (recall) =>
        recall.reason === "Target entity file was excluded by the path filters",
    ),
  );
  assert.ok(
    fixture.calls.fts.some((call) =>
      call.filter?.symbolNames?.includes("Namespace::AlphaSymbol"),
    ),
  );
  assert.equal(fixture.calls.vector.length >= 2, true);
  assert.ok(result.timings.some((entry) => entry.name === "search_total"));
});

test("search plans short-circuit empty path filters and force-track no-file reasons", async () => {
  const fixture = createFixture();
  const result = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query: "nothing" },
        { mode: "vector", query: "nothing" },
      ],
      includePaths: ["missing/**"],
      trackEntityId: "entity-a",
      trace: true,
    },
    fixture.context,
  );
  assert.equal(fixture.calls.fts.length, 0);
  assert.equal(fixture.calls.vector.length, 0);
  assert.equal(result.hits.length, 1);
  assert.ok(
    result.hits[0].trace.recall.every(
      (recall) => recall.reason === "No files matched the path filters",
    ),
  );
});

test("indexed rg-style globs match nested basenames and honor later overrides", async () => {
  const fixture = createFixture();
  const result = await searchWorkspaceIndex(
    {
      routes: [{ mode: "fts", query: "symbol" }],
      globs: ["!*.ts", "a.ts"],
    },
    fixture.context,
  );

  assert.deepEqual(
    result.hits.map((hit) => hit.file.relativePath),
    ["src/a.ts"],
  );
});

test("indexed path filtering yields during large searches and rejects excessive rule counts", async () => {
  const fixture = createFixture();
  const files = Array.from({ length: 5000 }, (_, index) =>
    file(`file-${index}`, `src/module-${index}.ts`),
  );
  fixture.context.storage.listFiles = () => files;
  let eventLoopRan = false;
  const immediate = setImmediate(() => {
    eventLoopRan = true;
  });
  try {
    const result = await searchWorkspaceIndex(
      { routes: [{ mode: "fts", query: "symbol" }], globs: ["*.missing"] },
      fixture.context,
    );
    assert.equal(result.hits.length, 0);
    assert.equal(eventLoopRan, true);
  } finally {
    clearImmediate(immediate);
  }
  await assert.rejects(
    searchWorkspaceIndex(
      {
        routes: [{ mode: "fts", query: "symbol" }],
        globs: Array(10001).fill("*.ts"),
      },
      fixture.context,
    ),
    /rule limit/,
  );
});

test("entity and file diagnosis handle missing targets and fallback entity selection", async () => {
  const fixture = createFixture();
  await assert.rejects(
    diagnoseEntitySearch("query", "missing", fixture.context),
    /Entity not found/,
  );
  assert.equal(
    await diagnoseFileSearch("query", "/repo/missing.ts", fixture.context),
    null,
  );
  const diagnosis = await diagnoseFileSearch(
    "query",
    fixture.files[0].absolutePath,
    fixture.context,
  );
  assert.equal(diagnosis.entityId, "entity-a");
  assert.equal(diagnosis.file.id, "file-a");

  const emptyFixture = createFixture();
  emptyFixture.storage.searchFts = () => [];
  emptyFixture.storage.searchVector = () => [];
  const fallback = await diagnoseFileSearch(
    "query",
    emptyFixture.files[1].absolutePath,
    emptyFixture.context,
  );
  assert.equal(fallback.entityId, "entity-b");

  emptyFixture.storage.listEntitiesByFile = () => [];
  assert.equal(
    await diagnoseFileSearch(
      "query",
      emptyFixture.files[1].absolutePath,
      emptyFixture.context,
    ),
    null,
  );
});

test("absolute path filters charge the budget from the matched path representation", async () => {
  const longDir = `/${"d".repeat(3000)}`;
  const files = [
    { ...file("file-abs", "f.ts"), absolutePath: `${longDir}/f.ts` },
    file("file-rel", "src/g.ts"),
  ];
  const storage = {
    listFiles: () => files,
    getFileById: (id) => files.find((item) => item.id === id) ?? null,
    searchFts: (query, limit, filter) =>
      files
        .filter((item) => !filter?.fileIds || filter.fileIds.includes(item.id))
        .map((item, index) => ({
          fragment: {
            id: `frag-${item.id}`,
            fileId: item.id,
            range: { kind: "text", startLine: 1, endLine: 2 },
            content: "value",
            metadata: {
              symbolName: "Symbol",
              symbolType: "function_declaration",
            },
          },
          file: item,
          path: "fts",
          score: 1 - index * 0.1,
        })),
    searchVector: () => [],
    optimize: () => {},
    close: () => {},
  };
  const context = {
    workspaceIndex: {
      id: "wi",
      name: "docs",
      path: "/tmp/index",
      rootPaths: [{ absolutePath: longDir, recursive: true }],
      createdTime: 1,
      updatedTime: 1,
    },
    storage,
    embeddingModel: new FakeEmbeddingModel(),
  };
  const absolute = await searchWorkspaceIndex(
    {
      routes: [{ mode: "fts", query: "value" }],
      includePaths: [`${longDir}/**`],
    },
    context,
  );
  assert.ok(absolute.hits.length >= 1);
  assert.ok(absolute.hits.every((hit) => hit.file.id === "file-abs"));
  const relative = await searchWorkspaceIndex(
    { routes: [{ mode: "fts", query: "value" }], includePaths: ["f.ts"] },
    context,
  );
  assert.ok(relative.hits.length >= 1);
  assert.ok(relative.hits.every((hit) => hit.file.id === "file-abs"));
});

test("pattern failures carry their original field and request index", async () => {
  const { context } = createFixture();
  await assert.rejects(
    searchWorkspaceIndex(
      { routes: [{ mode: "fts", query: "value" }], excludePaths: ["[z-a]"] },
      context,
    ),
    (error) =>
      /excludePaths\[0\]/.test(error.message) && /z-a/.test(error.message),
  );
  await assert.rejects(
    searchWorkspaceIndex(
      {
        routes: [{ mode: "fts", query: "value" }],
        excludePaths: ["", "[z-a]"],
      },
      context,
    ),
    (error) => /excludePaths\[1\]/.test(error.message),
  );
  await assert.rejects(
    searchWorkspaceIndex(
      {
        routes: [{ mode: "fts", query: "value" }],
        includePaths: ["keep.ts", "", "[z-a]"],
      },
      context,
    ),
    (error) => /includePaths\[2\]/.test(error.message),
  );
});

test("many absolute filters against a long path stay within the budget", async () => {
  const longDir = `/${"d".repeat(3000)}`;
  const files = [
    { ...file("file-abs", "f.ts"), absolutePath: `${longDir}/f.ts` },
  ];
  const storage = {
    listFiles: () => files,
    getFileById: (id) => files.find((item) => item.id === id) ?? null,
    searchFts: (query, limit, filter) =>
      files
        .filter((item) => !filter?.fileIds || filter.fileIds.includes(item.id))
        .map((item) => ({
          fragment: {
            id: `frag-${item.id}`,
            fileId: item.id,
            range: { kind: "text", startLine: 1, endLine: 2 },
            content: "value",
            metadata: {
              symbolName: "Symbol",
              symbolType: "function_declaration",
            },
          },
          file: item,
          path: "fts",
          score: 1,
        })),
    searchVector: () => [],
    optimize: () => {},
    close: () => {},
  };
  const context = {
    workspaceIndex: {
      id: "wi",
      name: "docs",
      path: "/tmp/index",
      rootPaths: [{ absolutePath: longDir, recursive: true }],
      createdTime: 1,
      updatedTime: 1,
    },
    storage,
    embeddingModel: new FakeEmbeddingModel(),
  };
  const absoluteFilters = [
    ...Array.from({ length: 4999 }, () => "/nomatch"),
    "/**",
  ];
  const result = await searchWorkspaceIndex(
    {
      routes: [{ mode: "fts", query: "value" }],
      includePaths: absoluteFilters,
    },
    context,
  );
  assert.ok(result.hits.length >= 1);
  assert.ok(result.hits.every((hit) => hit.file.id === "file-abs"));
});

test("oversized glob filters report their field and original index", async () => {
  const { context } = createFixture();
  const oversized = "x".repeat(4097);
  await assert.rejects(
    searchWorkspaceIndex(
      { routes: [{ mode: "fts", query: "value" }], globs: [oversized] },
      context,
    ),
    (error) =>
      /globs\[0\]/.test(error.message) && /4096-character/.test(error.message),
  );
  await assert.rejects(
    searchWorkspaceIndex(
      {
        routes: [{ mode: "fts", query: "value" }],
        insensitiveGlobs: ["ok.ts", oversized],
      },
      context,
    ),
    (error) =>
      /insensitiveGlobs\[1\]/.test(error.message) &&
      /4096-character/.test(error.message),
  );
});

test("search candidate evaluation yields under heavy slash-bearing admitted globs", async () => {
  // Runs in a worker thread: the heartbeat monitor measures the worker's own
  // event loop (the loop the stall blocks), while the parent enforces the
  // external deadline and terminates the worker on timeout.
  await inSearchWorker(
    `
    const files = [
      {
        id: "file-a",
        absolutePath: "/repo/" + "s".repeat(990) + "/a.ts",
        relativePath: "s".repeat(990) + "/a.ts",
        rootPath: "/repo",
        sizeBytes: 10,
        lastModifiedTime: 100,
        kind: "code",
        format: "typescript",
      },
    ];
    const storage = {
      listFiles: () => files,
      getFileById: (id) => files.find((item) => item.id === id) ?? null,
      searchFts: (query, limit, filter) =>
        files
          .filter(
            (item) => !filter?.fileIds || filter.fileIds.includes(item.id),
          )
          .map((item) => ({
            fragment: {
              id: "frag-" + item.id,
              fileId: item.id,
              range: { kind: "text", startLine: 1, endLine: 2 },
              content: "value",
              metadata: {
                symbolName: "Symbol",
                symbolType: "function_declaration",
              },
            },
            file: item,
            path: "fts",
            score: 1,
          })),
      searchVector: () => [],
      optimize: () => {},
      close: () => {},
    };
    const context = {
      workspaceIndex: {
        id: "wi",
        name: "docs",
        path: "/tmp/index",
        rootPaths: [{ absolutePath: "/repo", recursive: true }],
        createdTime: 1,
        updatedTime: 1,
      },
      storage,
      embeddingModel: new FakeEmbeddingModel(),
    };
    const heavyGlobs = Array.from(
      { length: 100 },
      (_, i) => "*s".repeat(150) + "/b" + i + "*",
    );
    let maxGap = 0;
    let last = Date.now();
    let timer;
    const tick = () => {
      const now = Date.now();
      maxGap = Math.max(maxGap, now - last);
      last = now;
    };
    try {
      timer = setInterval(tick, 5);
      await new Promise((resolve) => setTimeout(resolve, 20));
      last = Date.now();
      let hits = -1;
      await search(
        {
          routes: [{ mode: "fts", query: "value" }],
          globs: [...heavyGlobs, "*a.ts"],
        },
        context,
      ).then((result) => {
        hits = result.hits.length;
      });
      assert.ok(hits >= 1, "expected the file to match");
      assert.ok(
        maxGap < 250,
        "max event-loop block was " + maxGap + "ms",
      );
    } finally {
      clearInterval(timer);
      maxGap = Math.max(maxGap, Date.now() - last);
    }
    assert.ok(maxGap < 250, "max event-loop block (drained) was " + maxGap + "ms");
  `,
    undefined,
    120_000,
  );
});

function searchCancellationFixture() {
  const files = [file("file-a", "src/a.ts")];
  const storage = {
    listFiles: () => files,
    getFileById: (id) => files.find((item) => item.id === id) ?? null,
    searchFts: () => [],
    searchVector: () => [],
    optimize: () => {},
    close: () => {},
  };
  return {
    context: {
      workspaceIndex: {
        id: "wi",
        name: "docs",
        path: "/tmp/index",
        rootPaths: [{ absolutePath: "/repo", recursive: true }],
        createdTime: 1,
        updatedTime: 1,
      },
      storage,
      embeddingModel: new FakeEmbeddingModel(),
    },
  };
}

test("pre-aborted search rejects at entry preserving its reason", async () => {
  const { context } = searchCancellationFixture();
  const reason = new Error("stop-entry");
  const pre = new AbortController();
  pre.abort(reason);
  await assert.rejects(
    searchWorkspaceIndex(
      { routes: [{ mode: "vector", query: "value" }] },
      context,
      { signal: pre.signal },
    ),
    (error) => error === reason,
  );
});

test("unfiltered search aborting after entry rejects before success", async () => {
  const { context } = searchCancellationFixture();
  const reason = new Error("stop-after-entry");
  const controller = new AbortController();
  // The entry check runs synchronously at call time; aborting immediately
  // after the call delivers the signal after entry but before any result.
  const pending = searchWorkspaceIndex(
    { routes: [{ mode: "vector", query: "value" }] },
    context,
    { signal: controller.signal },
  );
  controller.abort(reason);
  await assert.rejects(pending, (error) => error === reason);
});

test("abort during embedding rejects preserving its reason", async () => {
  const { context } = searchCancellationFixture();
  const controller = new AbortController();
  const reason = new Error("stop-embedding");
  const slowModel = new FakeEmbeddingModel();
  const originalEmbed = slowModel.embed.bind(slowModel);
  slowModel.embed = async (...args) => {
    controller.abort(reason);
    await new Promise((resolve) => setImmediate(resolve));
    return originalEmbed(...args);
  };
  await assert.rejects(
    searchWorkspaceIndex(
      { routes: [{ mode: "vector", query: "value" }] },
      { ...context, embeddingModel: slowModel },
      { signal: controller.signal },
    ),
    (error) => error === reason,
  );
});
