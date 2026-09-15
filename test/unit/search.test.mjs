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

test("exact symbol definitions outrank candidates with two weak recall votes", async () => {
  const fixture = createFixture();
  const hit = (index, path) => ({
    fragment: fragment(fixture.entities[index]),
    file: fixture.files[index],
    path,
    score: 1,
  });
  fixture.storage.searchFts = () => [hit(0, "fts"), hit(1, "fts")];
  fixture.storage.searchVector = () => [hit(1, "vector")];
  const result = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query: "AlphaSymbol" },
        { mode: "vector", query: "AlphaSymbol" },
      ],
    },
    fixture.context,
  );
  assert.equal(result.hits[0].entity.id, "entity-a");
  assert.equal(result.hits[0].matchedBy, "fts");
  assert.ok(
    result.hits[1].score > result.hits[0].score,
    "RRF remains a rank-fusion score, not confidence",
  );
});

test("exact text in a recalled fragment outranks weak dual-route hits even when its entity is an outline", async () => {
  const fixture = createFixture();
  const source = fragment(fixture.entities[0]);
  source.content = { kind: "text", text: 'throw new Error("No index found")' };
  fixture.storage.searchFts = () => [
    { fragment: source, file: fixture.files[0], path: "fts", score: 1 },
    {
      fragment: fragment(fixture.entities[1]),
      file: fixture.files[1],
      path: "fts",
      score: 1,
    },
  ];
  fixture.storage.searchVector = () => [
    {
      fragment: fragment(fixture.entities[1]),
      file: fixture.files[1],
      path: "vector",
      score: 1,
    },
  ];
  const result = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query: "No index found" },
        { mode: "vector", query: "No index found" },
      ],
    },
    fixture.context,
  );
  assert.equal(result.hits[0].entity.id, "entity-a");
  assert.equal(result.hits[0].matchedBy, "fts");
});

test("mixed questions retain semantic ranking rather than boosting every mentioned symbol", async () => {
  const fixture = createFixture();
  const hit = (index, path) => ({
    fragment: fragment(fixture.entities[index]),
    file: fixture.files[index],
    path,
    score: 1,
  });
  fixture.storage.searchFts = () => [hit(0, "fts"), hit(1, "fts")];
  fixture.storage.searchVector = () => [hit(1, "vector")];
  const query = "how does AlphaSymbol retry failed requests";
  const result = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query },
        { mode: "vector", query },
      ],
    },
    fixture.context,
  );
  assert.equal(result.hits[0].entity.id, "entity-b");
});

test("hybrid text search uses filename and code-subword support without changing explicit routes", async () => {
  const fixture = createFixture();
  fixture.files[0].relativePath = "src/input-budget.ts";
  fixture.entities[0].metadata.symbolName = "indexChunkOptions";
  fixture.entities[0].content.text =
    "function indexChunkOptions(maxInputTokens) { return maxInputTokens; }";
  const hit = (index, path) => ({
    fragment: fragment(fixture.entities[index]),
    file: fixture.files[index],
    path,
    score: 1,
  });
  fixture.storage.searchFts = () => [hit(1, "fts"), hit(2, "fts")];
  fixture.storage.searchVector = () => [
    hit(2, "vector"),
    hit(0, "vector"),
    hit(1, "vector"),
  ];
  const query = "input token budget";
  const hybrid = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query },
        { mode: "vector", query },
      ],
      trace: true,
    },
    fixture.context,
  );
  assert.equal(hybrid.hits[0].entity.id, "entity-a");
  assert.equal(
    hybrid.hits[0].matchedBy,
    "vector",
    "lexical support is not a fabricated FTS recall",
  );
  assert.ok(hybrid.hits[0].trace.ranking.lexicalSupport > 0);
  assert.ok(
    hybrid.hits[0].trace.ranking.score > hybrid.hits[0].trace.fusion.score,
  );
  for (const mode of ["fts", "vector"]) {
    const explicit = await searchWorkspaceIndex(
      { routes: [{ mode, query }], trace: true },
      fixture.context,
    );
    assert.equal(
      explicit.hits[0].entity.id,
      mode === "fts" ? "entity-b" : "entity-c",
    );
    assert.equal(explicit.hits[0].score, 1 / 61);
    assert.equal(explicit.hits[0].trace.ranking, undefined);
  }
  const separate = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query },
        { mode: "vector", query: "find shared settings" },
      ],
      trace: true,
    },
    fixture.context,
  );
  assert.equal(separate.hits[0].entity.id, "entity-c");
  assert.equal(separate.hits[0].trace.ranking, undefined);
});

test("excluded tracked entities do not change hybrid support or the rank constant", async () => {
  for (const hasOrdinarySupport of [false, true]) {
    const fixture = createFixture();
    fixture.files[2].relativePath = "docs/token-budget.ts";
    if (hasOrdinarySupport) fixture.entities[0].content.text = "token";
    const plan = {
      routes: [
        { mode: "fts", query: "token budget" },
        { mode: "vector", query: "token budget" },
      ],
      includePaths: ["src"],
      trace: true,
    };
    const ordinary = await searchWorkspaceIndex(plan, fixture.context);
    const tracked = await searchWorkspaceIndex(
      { ...plan, trackEntityId: "entity-c" },
      fixture.context,
    );
    assert.ok(
      tracked.trackedHit.trace.recall.every(
        (recall) => recall.forced && !recall.found,
      ),
    );
    assert.deepEqual(
      tracked.hits.filter((hit) => hit.entity.id !== "entity-c"),
      ordinary.hits,
      `diagnostic-only support must not affect ordinary hits (${hasOrdinarySupport})`,
    );
  }
});

test("forced route evidence does not enable lexical support on an ordinary candidate", async () => {
  const fixture = createFixture();
  const hit = (index, path) => ({
    fragment: fragment(fixture.entities[index]),
    file: fixture.files[index],
    path,
    score: 1,
  });
  fixture.storage.searchFts = () => [hit(0, "fts")];
  fixture.storage.searchVector = (_vector, _limit, filter) => {
    if (!filter?.groupIds) return [hit(1, "vector")];
    return [
      {
        ...hit(0, "vector"),
        fragment: {
          ...fragment(fixture.entities[0], {
            id: "diagnostic-fragment",
            group: "entity-a",
          }),
          content: { kind: "text", text: "token" },
        },
      },
    ];
  };
  const result = await searchWorkspaceIndex(
    {
      routes: [
        { mode: "fts", query: "token budget" },
        { mode: "vector", query: "token budget" },
      ],
      trackEntityId: "entity-a",
      trace: true,
    },
    fixture.context,
  );
  assert.ok(result.trackedHit.evidence.some((item) => item.forced));
  assert.equal(result.trackedHit.trace.ranking, undefined);
  assert.equal(
    result.trackedHit.score,
    1 / 61 + 1 / 261,
    "existing diagnostic recall votes retain the ordinary RRF constant",
  );
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
