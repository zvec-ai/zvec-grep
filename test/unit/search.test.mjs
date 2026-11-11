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

test("semantic full traces are bounded, provenance-bearing, deterministic, and body-free", async () => {
  const fixture = createFixture();
  const semanticFeatures = {
    semantic_features_version: 1,
    representation: "all",
    token_sketch: "trace",
    graph_rerank: "shadow",
    graph_edge_confidence: "strong",
    graph_expansion: "off",
    trace: "full",
  };
  const plan = {
    routes: [{ mode: "fts", query: "AlphaSymbol" }],
    limit: 1,
    semanticFeatures,
  };
  const first = await searchWorkspaceIndex(plan, fixture.context);
  const second = await searchWorkspaceIndex(plan, fixture.context);

  assert.deepEqual(first.plan.semanticFeatures, semanticFeatures);
  assert.deepEqual(first.trace, second.trace);
  assert.equal(first.trace.schema_version, 1);
  assert.match(first.trace.generation_id, /^sha256:/);
  assert.match(first.trace.source_digest, /^sha256:/);
  assert.match(first.trace.source_selection_digest, /^sha256:/);
  assert.match(first.trace.artifacts.index_sha256, /^sha256:/);
  assert.match(first.trace.artifacts.embedding_schema, /^sha256:/);
  assert.equal(first.trace.artifacts.graph_generation_id, null);
  assert.equal(first.trace.artifacts.graph_artifact_sha256, null);
  assert.equal(first.trace.artifacts.graph_publication_sha256, null);
  assert.equal(first.trace.source_files, fixture.files.length);
  assert.equal(first.trace.source_bytes, 30);
  assert.equal(first.trace.candidate_count, 3);
  assert.equal(first.trace.candidates.length, 3);
  assert.equal(first.trace.candidates[0].entity_id, "entity-a");
  assert.deepEqual(first.trace.graph, {
    binding: "unbound",
    generation_id: null,
    artifact_sha256: null,
    publication_sha256: null,
    seed_ids: [],
    eligible_edges: 0,
    excluded_edges: { unresolved: 0, ambiguous: 0, stale: 0 },
  });
  assert.deepEqual(first.trace.expansion, {
    enabled: false,
    related_count: 0,
    items: [],
  });
  assert.equal(first.trace.candidates[0].graph_evidence.length, 0);
  assert.equal(JSON.stringify(first.trace).includes("export function"), false);
});

test("semantic feature validation rejects unsupported versions and enum values", async () => {
  const fixture = createFixture();
  const base = {
    semantic_features_version: 1,
    representation: "all",
    token_sketch: "off",
    graph_rerank: "off",
    graph_edge_confidence: "strong",
    graph_expansion: "off",
    trace: "off",
  };
  await assert.rejects(
    searchWorkspaceIndex(
      {
        routes: [{ mode: "fts", query: "AlphaSymbol" }],
        semanticFeatures: { ...base, semantic_features_version: 2 },
      },
      fixture.context,
    ),
    /Unsupported semantic features version/,
  );
  await assert.rejects(
    searchWorkspaceIndex(
      {
        routes: [{ mode: "fts", query: "AlphaSymbol" }],
        semanticFeatures: { ...base, graph_rerank: "invalid" },
      },
      fixture.context,
    ),
    /Unsupported semantic feature value/,
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
