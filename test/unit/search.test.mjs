import assert from "node:assert/strict";
import test from "node:test";
import {
  diagnoseEntitySearch,
  diagnoseFileSearch,
  searchWorkspaceIndex,
} from "../../dist/engine/pipeline/search/index.js";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";

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

test("limit-based weighting skips tail candidates without changing results", async () => {
  const { context } = createFixture();

  // The visible window is the contract: pruning may leave tail candidates
  // unweighted, but everything the caller sees must be identical.
  const limited = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query: "Symbol" },
        { mode: "vector", query: "Symbol" },
      ],
      limit: 1,
    },
    context,
  );
  const full = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query: "Symbol" },
        { mode: "vector", query: "Symbol" },
      ],
      limit: 99,
    },
    context,
  );

  assert.equal(limited.hits.length, 1);
  assert.equal(limited.hits[0].entity.id, full.hits[0].entity.id);
  assert.equal(limited.hits[0].score, full.hits[0].score);
});

test("skips scoring candidates that cannot reach the visible window", async () => {
  // The three-entity fixture keeps every candidate within reach of the cutoff,
  // so it never exercises the skip itself. Build a deep tail instead, and make
  // symbolType observable to prove the tail is not merely ranked low but never
  // scored at all.
  const TAIL = 200;
  const files = [];
  const entities = [];
  const scored = new Set();

  for (let index = 0; index < TAIL; index++) {
    const id = `entity-${String(index).padStart(3, "0")}`;
    files.push(file(`file-${index}`, `src/f${index}.ts`, 100));

    const base = entity(id, `file-${index}`, `Symbol${index}`);
    // Reading symbolType is the first thing scoring does with code metadata.
    entities.push({
      ...base,
      metadata: Object.defineProperty({ ...base.metadata }, "symbolType", {
        get() {
          scored.add(id);
          return "function";
        },
        enumerable: true,
      }),
    });
  }

  const fileOf = (item) =>
    files.find((candidate) => candidate.id === item.fileId);
  const ranked = (path) =>
    entities.map((item, index) => ({
      fragment: fragment(item),
      file: fileOf(item),
      path,
      score: 1 - index * 0.001,
    }));

  const storage = {
    getFileById: (id) => files.find((item) => item.id === id) ?? null,
    getFileByPath: (path) =>
      files.find((item) => item.absolutePath === path) ?? null,
    listFiles: () => files,
    listEntitiesByFile: () => [],
    getEntity: (id) => {
      const found = entities.find((item) => item.id === id);
      if (!found) return null;
      return { entity: found, file: fileOf(found), vector: [1, 0] };
    },
    upsertFile: () => {},
    markFileFailed: () => {},
    deleteFile: () => {},
    searchFts: (_query, limit) => ranked("fts").slice(0, limit),
    searchVector: (_vector, limit) => ranked("vector").slice(0, limit),
    optimize: () => {},
    close: () => {},
  };

  const context = {
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
  };

  const routes = [
    { mode: "fts", query: "Symbol" },
    { mode: "vector", query: "Symbol" },
  ];

  const limited = await searchWorkspaceIndex({ routes, limit: 5 }, context);
  assert.equal(limited.hits.length, 5);

  const deepTail = `entity-${String(TAIL - 1).padStart(3, "0")}`;
  assert.ok(
    !scored.has(deepTail),
    `${deepTail} cannot reach the window and should never be scored`,
  );
  assert.ok(
    scored.size < TAIL,
    `pruning should skip the tail, but all ${TAIL} candidates were scored`,
  );

  // The window itself must still match a full, unpruned scoring pass.
  scored.clear();
  const full = await searchWorkspaceIndex({ routes, limit: TAIL }, context);
  assert.deepEqual(
    limited.hits.map((hit) => hit.entity.id),
    full.hits.slice(0, 5).map((hit) => hit.entity.id),
  );
});

test("tracking an entity still reports a weighted score for it", async () => {
  const { context } = createFixture();

  // Tracking must bypass pruning: the tracked entity can sit outside the
  // window, and reporting an unweighted score for it would be misleading.
  const tracked = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query: "Symbol" },
        { mode: "vector", query: "Symbol" },
      ],
      limit: 1,
      trackEntityId: "entity-c",
    },
    context,
  );
  const full = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query: "Symbol" },
        { mode: "vector", query: "Symbol" },
      ],
      limit: 99,
    },
    context,
  );

  const fromFull = full.hits.find((hit) => hit.entity.id === "entity-c");
  assert.ok(tracked.trackedHit, "tracked hit should be present");
  assert.equal(tracked.trackedHit.score, fromFull.score);
});

test("populates distinct fusion and ranking traces when ranking reweights hits", async () => {
  const { context } = createFixture();

  // Search with trace enabled
  const result = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query: "AlphaSymbol" },
        { mode: "vector", query: "AlphaSymbol" },
      ],
      limit: 3,
      trace: true,
    },
    context,
  );

  assert.ok(result.hits.length > 0, "hits should be returned");
  const hitWithTrace = result.hits[0];
  assert.ok(hitWithTrace.trace, "trace should be present on hit");
  assert.ok(hitWithTrace.trace.fusion, "fusion trace should be present");
  assert.ok(hitWithTrace.trace.final, "final trace should be present");

  // Since AlphaSymbol matches exact symbol name, it receives ranking weight > 1.
  // The fusion trace records pre-weighting RRF score and ranking trace records post-weighting.
  if (hitWithTrace.trace.ranking) {
    assert.ok(
      hitWithTrace.trace.ranking.score >= hitWithTrace.trace.fusion.score,
      `ranking score (${hitWithTrace.trace.ranking.score}) should reflect multiplier over fusion score (${hitWithTrace.trace.fusion.score})`,
    );
  }
});

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
