import assert from "node:assert/strict";
import test from "node:test";
import {
  SqliteGraphStorage,
  queryGraphNeighborhood,
} from "../../dist/engine/graph/index.js";
import { searchWorkspaceIndex } from "../../dist/engine/pipeline/search/index.js";

function entity(id, name, path = "a.ts") {
  return {
    file: {
      id: `file-${path}`,
      collectionId: "c",
      absolutePath: `/repo/${path}`,
      relativePath: path,
      rootPath: "/repo",
      sizeBytes: 1,
      lastModifiedTime: 1,
      kind: "code",
      format: "typescript",
    },
    entity: {
      id,
      fileId: `file-${path}`,
      range: {
        kind: "text",
        startLine: 1,
        endLine: 3,
        startOffset: 0,
        endOffset: 10,
      },
      content: { kind: "text", text: `function ${name}() {\n  return 1;\n}` },
      metadata: {
        kind: "code",
        symbolType: "function",
        symbolName: name,
        scope: null,
        nodeType: "function_declaration",
        signature: `function ${name}()`,
        doc: null,
        modifiers: ["exported"],
      },
    },
  };
}

test("queryGraphNeighborhood enriches callers from storage", () => {
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(
    "fa",
    [
      { id: "caller", kind: "function", is_exported: true, name: "caller" },
      { id: "target", kind: "function", is_exported: true, name: "target" },
    ],
    [
      {
        src: "caller",
        dst: "target",
        rel: "call",
        count: 2,
        first_line: 2,
        ref_name: "target",
        kind: "CALLS",
      },
    ],
    [],
  );

  const entities = new Map([
    ["caller", entity("caller", "caller")],
    ["target", entity("target", "target")],
  ]);
  const storage = {
    findSymbolsByName(name) {
      return [...entities.values()].filter(
        (item) => item.entity.metadata.symbolName === name,
      );
    },
    getEntity(id) {
      return entities.get(id) ?? null;
    },
  };

  const result = queryGraphNeighborhood(graph, storage, {
    direction: "callers",
    query: "target",
  });

  assert.equal(result.available, true);
  assert.equal(result.seed?.id, "target");
  assert.equal(result.neighbors.length, 1);
  assert.equal(result.neighbors[0].id, "caller");
  assert.equal(
    result.neighbors[0].entity?.entity.metadata.symbolName,
    "caller",
  );

  const callees = queryGraphNeighborhood(graph, storage, {
    direction: "callees",
    query: "caller",
  });
  assert.equal(callees.neighbors[0]?.id, "target");
  assert.equal(callees.neighbors[0]?.count, 2);
  graph.close();
});

function indexedFile(id, relativePath) {
  return {
    id,
    collectionId: "c",
    absolutePath: `/repo/${relativePath}`,
    relativePath,
    rootPath: "/repo",
    sizeBytes: 1,
    lastModifiedTime: 1,
    kind: "code",
    format: "typescript",
  };
}

test("searchWorkspaceIndex does not graph-boost an isolated seed", async () => {
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(
    "file-a.ts",
    [{ id: "seed", kind: "function", is_exported: true, name: "seed" }],
    [],
    [],
  );
  const seed = entity("seed", "seed", "a.ts");
  const storage = {
    getEntity(id) {
      return id === "seed" ? seed : null;
    },
    listEntitiesByFile() {
      return [seed];
    },
    searchFts(_query, limit) {
      return [
        {
          path: "fts",
          score: 1,
          file: seed.file,
          fragment: {
            id: seed.entity.id,
            fileId: seed.entity.fileId,
            range: seed.entity.range,
            content: seed.entity.content,
            metadata: seed.entity.metadata,
          },
        },
      ].slice(0, limit);
    },
    searchVector() {
      return [];
    },
    getFileById() {
      return null;
    },
    getFileByPath() {
      return null;
    },
    listFilesByPathPrefix() {
      return [];
    },
    listFiles() {
      return [];
    },
    upsertFile() {},
    markFileFailed() {},
    deleteFile() {},
    async optimize() {},
    close() {},
  };

  const result = await searchWorkspaceIndex(
    { routes: [{ mode: "fts", query: "seed" }], limit: 10, trace: true },
    {
      workspaceIndex: { id: "c", name: "c", path: "/tmp/c" },
      storage,
      graph,
    },
  );

  assert.equal(result.graphExpand?.neighborsAdded, 0);
  assert.equal(
    result.hits[0].trace.recall.some(
      (recall) => recall.routeId === "graph.explore",
    ),
    false,
  );
  graph.close();
});

test("searchWorkspaceIndex silently expands IMPORTS file neighbors into hits", async () => {
  const graph = new SqliteGraphStorage("", { inMemory: true });
  const fileA = indexedFile("file-a.ts", "a.ts");
  const fileB = indexedFile("file-b.ts", "b.ts");

  graph.upsertFileGraph(
    fileA.id,
    [{ id: "seed", kind: "function", is_exported: true, name: "seed" }],
    [],
    [
      {
        owner: fileA.id,
        id: "ref-import-b",
        ref_name: "./b",
        ref_kind: "import",
        line: 1,
        owner_is_file: true,
      },
    ],
  );
  graph.upsertFileGraph(
    fileB.id,
    [{ id: "utilFn", kind: "function", is_exported: true, name: "utilFn" }],
    [],
    [],
  );
  await graph.resolvePending({ files: [fileA, fileB] });
  assert.deepEqual(
    graph.expandFileNeighbors([fileA.id], 10).map((n) => n.id),
    [fileB.id],
  );

  const seed = entity("seed", "seed", "a.ts");
  const util = entity("utilFn", "utilFn", "b.ts");
  const byId = new Map([
    ["seed", seed],
    ["utilFn", util],
  ]);
  const byFile = new Map([
    [fileA.id, [seed]],
    [fileB.id, [util]],
  ]);

  const storage = {
    getEntity(id) {
      return byId.get(id) ?? null;
    },
    listEntitiesByFile(fileId) {
      return byFile.get(fileId) ?? [];
    },
    searchFts(query, limit) {
      if (query.includes("seed") || query === "seedFn") {
        return [
          {
            path: "fts",
            score: 1,
            file: seed.file,
            fragment: {
              id: seed.entity.id,
              fileId: seed.entity.fileId,
              range: seed.entity.range,
              content: seed.entity.content,
              metadata: seed.entity.metadata,
            },
          },
        ].slice(0, limit);
      }
      return [];
    },
    searchVector() {
      return [];
    },
    getFileById() {
      return null;
    },
    getFileByPath() {
      return null;
    },
    listFilesByPathPrefix() {
      return [];
    },
    listFiles() {
      return [];
    },
    upsertFile() {},
    markFileFailed() {},
    deleteFile() {},
    async optimize() {},
    close() {},
  };

  const result = await searchWorkspaceIndex(
    { routes: [{ mode: "fts", query: "seedFn" }], limit: 10 },
    {
      workspaceIndex: { id: "c", name: "c", path: "/tmp/c" },
      storage,
      graph,
    },
  );

  assert.equal(result.graphExpand?.available, true);
  assert.ok(result.graphExpand?.neighborsAdded >= 1);
  const neighborHit = result.hits.find((hit) => hit.entity.id === "utilFn");
  assert.ok(neighborHit, "IMPORTS neighbor entity should appear in hits");
  assert.equal(neighborHit.matchedBy, "graph");
  assert.ok(
    result.relationships.some((relation) => relation.kind === "IMPORTS"),
  );
  const importRelation = result.relationships.find(
    (relation) => relation.kind === "IMPORTS",
  );
  assert.equal(importRelation.srcId, fileA.id);
  assert.equal(importRelation.dstId, fileB.id);
  graph.close();
});

test("searchWorkspaceIndex preserves incoming IMPORTS direction", async () => {
  const graph = new SqliteGraphStorage("", { inMemory: true });
  const fileA = indexedFile("file-a.ts", "a.ts");
  const fileB = indexedFile("file-b.ts", "b.ts");
  graph.upsertFileGraph(
    fileA.id,
    [{ id: "seed", kind: "function", is_exported: true, name: "seed" }],
    [],
    [],
  );
  graph.upsertFileGraph(
    fileB.id,
    [{ id: "caller", kind: "function", is_exported: true, name: "caller" }],
    [],
    [
      {
        owner: fileB.id,
        id: "ref-import-a",
        ref_name: "./a",
        ref_kind: "import",
        line: 1,
        owner_is_file: true,
      },
    ],
  );
  await graph.resolvePending({ files: [fileA, fileB] });

  const seed = entity("seed", "seed", "a.ts");
  const caller = entity("caller", "caller", "b.ts");
  const byId = new Map([
    ["seed", seed],
    ["caller", caller],
  ]);
  const storage = {
    getEntity(id) {
      return byId.get(id) ?? null;
    },
    listEntitiesByFile(fileId) {
      return fileId === fileB.id ? [caller] : [seed];
    },
    searchFts() {
      return [
        {
          path: "fts",
          score: 1,
          file: seed.file,
          fragment: {
            id: seed.entity.id,
            fileId: seed.entity.fileId,
            range: seed.entity.range,
            content: seed.entity.content,
            metadata: seed.entity.metadata,
          },
        },
      ];
    },
    searchVector() {
      return [];
    },
    listFiles() {
      return [fileA, fileB];
    },
  };
  const result = await searchWorkspaceIndex(
    { routes: [{ mode: "fts", query: "seed" }], limit: 10 },
    { workspaceIndex: { id: "c", name: "c", path: "/tmp/c" }, storage, graph },
  );
  const relation = result.relationships.find((item) => item.kind === "IMPORTS");
  assert.equal(relation.srcId, fileB.id);
  assert.equal(relation.dstId, fileA.id);
  graph.close();
});

test("searchWorkspaceIndex silently expands CONTAINS container neighbors into hits", async () => {
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(
    "fa",
    [
      { id: "Parent", kind: "class", is_exported: true, name: "Parent" },
      { id: "seed", kind: "function", is_exported: true, name: "seed" },
      { id: "sib", kind: "function", is_exported: true, name: "sib" },
    ],
    [
      {
        src: "Parent",
        dst: "seed",
        rel: "contains",
        count: 1,
        first_line: 0,
        ref_name: "seed",
        kind: "CONTAINS",
      },
      {
        src: "Parent",
        dst: "sib",
        rel: "contains",
        count: 1,
        first_line: 0,
        ref_name: "sib",
        kind: "CONTAINS",
      },
    ],
    [],
  );

  const seed = entity("seed", "seed");
  const parent = entity("Parent", "Parent");
  parent.entity.metadata.symbolType = "class";
  const sib = entity("sib", "sib");
  const byId = new Map([
    ["seed", seed],
    ["Parent", parent],
    ["sib", sib],
  ]);

  const storage = {
    getEntity(id) {
      return byId.get(id) ?? null;
    },
    listEntitiesByFile() {
      return [];
    },
    searchFts(query, limit) {
      if (query.includes("seed") || query === "seedFn") {
        return [
          {
            path: "fts",
            score: 1,
            file: seed.file,
            fragment: {
              id: seed.entity.id,
              fileId: seed.entity.fileId,
              range: seed.entity.range,
              content: seed.entity.content,
              metadata: seed.entity.metadata,
            },
          },
        ].slice(0, limit);
      }
      return [];
    },
    searchVector() {
      return [];
    },
    getFileById() {
      return null;
    },
    getFileByPath() {
      return null;
    },
    listFilesByPathPrefix() {
      return [];
    },
    listFiles() {
      return [];
    },
    upsertFile() {},
    markFileFailed() {},
    deleteFile() {},
    async optimize() {},
    close() {},
  };

  const result = await searchWorkspaceIndex(
    { routes: [{ mode: "fts", query: "seedFn" }], limit: 10 },
    {
      workspaceIndex: { id: "c", name: "c", path: "/tmp/c" },
      storage,
      graph,
    },
  );

  assert.ok(result.hits.some((hit) => hit.entity.id === "Parent"));
  assert.ok(result.hits.some((hit) => hit.entity.id === "sib"));
  assert.equal(
    result.hits.find((hit) => hit.entity.id === "Parent")?.matchedBy,
    "graph",
  );
  graph.close();
});

test("searchWorkspaceIndex silently expands call neighbors into hits", async () => {
  const graph = new SqliteGraphStorage("", { inMemory: true });
  graph.upsertFileGraph(
    "fa",
    [
      { id: "seed", kind: "function", is_exported: true, name: "seed" },
      { id: "nbr", kind: "function", is_exported: true, name: "nbr" },
      {
        id: "existing",
        kind: "function",
        is_exported: true,
        name: "existing",
      },
      { id: "excluded", kind: "class", is_exported: true, name: "excluded" },
    ],
    [
      {
        src: "seed",
        dst: "nbr",
        rel: "call",
        count: 3,
        first_line: 2,
        ref_name: "nbr",
        kind: "CALLS",
      },
      {
        src: "seed",
        dst: "existing",
        rel: "call",
        count: 1,
        first_line: 3,
        ref_name: "existing",
        kind: "CALLS",
      },
      {
        src: "seed",
        dst: "excluded",
        rel: "type",
        count: 1,
        first_line: 4,
        ref_name: "excluded",
        kind: "REFS",
      },
    ],
    [],
  );

  const seed = entity("seed", "seed");
  const nbr = entity("nbr", "nbr", "b.ts");
  const existing = entity("existing", "existing", "existing.ts");
  const excluded = entity("excluded", "excluded", "model.ts");
  excluded.entity.metadata.symbolType = "class";
  const byId = new Map([
    ["seed", seed],
    ["nbr", nbr],
    ["existing", existing],
    ["excluded", excluded],
  ]);

  const storage = {
    getEntity(id) {
      return byId.get(id) ?? null;
    },
    searchFts(query, limit) {
      if (query.includes("seed") || query === "seedFn") {
        return [
          {
            path: "fts",
            score: 1,
            file: seed.file,
            fragment: {
              id: seed.entity.id,
              fileId: seed.entity.fileId,
              range: seed.entity.range,
              content: seed.entity.content,
              metadata: seed.entity.metadata,
            },
          },
          {
            path: "fts",
            score: 0.8,
            file: existing.file,
            fragment: {
              id: existing.entity.id,
              fileId: existing.entity.fileId,
              range: existing.entity.range,
              content: existing.entity.content,
              metadata: existing.entity.metadata,
            },
          },
        ].slice(0, limit);
      }
      return [];
    },
    searchVector() {
      return [];
    },
    getFileById() {
      return null;
    },
    getFileByPath() {
      return null;
    },
    listFilesByPathPrefix() {
      return [];
    },
    listFiles() {
      return [];
    },
    listEntitiesByFile() {
      return [];
    },
    upsertFile() {},
    markFileFailed() {},
    deleteFile() {},
    async optimize() {},
    close() {},
  };

  const result = await searchWorkspaceIndex(
    {
      routes: [{ mode: "fts", query: "seedFn" }],
      limit: 10,
      symbolTypes: ["function"],
    },
    {
      workspaceIndex: { id: "c", name: "c", path: "/tmp/c" },
      storage,
      graph,
    },
  );

  assert.equal(result.graphExpand?.available, true);
  assert.ok(result.graphExpand?.neighborsAdded >= 1);
  const neighborHit = result.hits.find((hit) => hit.entity.id === "nbr");
  assert.ok(neighborHit);
  assert.equal(neighborHit.matchedBy, "graph");
  assert.ok(
    result.relationships.some(
      (relation) =>
        relation.kind === "CALLS" &&
        relation.srcLabel === "seed" &&
        relation.dstLabel === "nbr",
    ),
  );
  const existingHit = result.hits.find((hit) => hit.entity.id === "existing");
  assert.ok(
    existingHit?.evidence.some((evidence) => evidence.path === "graph"),
  );
  assert.equal(
    result.hits.some((hit) => hit.entity.id === "excluded"),
    false,
  );
  graph.close();
});
