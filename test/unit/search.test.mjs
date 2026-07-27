import assert from "node:assert/strict";
import test from "node:test";
import {
  diagnoseEntitySearch,
  diagnoseFileSearch,
  searchPlanCollection,
} from "../../dist/engine/pipeline/search/index.js";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";

function file(id, relativePath, lastModifiedTime = 100) {
  return {
    id,
    collectionId: "collection-1",
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

function rangedEntity(
  id,
  fileId,
  symbolName,
  {
    startOffset,
    endOffset,
    scope = null,
    symbolType = "function",
    text = `${symbolType} ${symbolName}`,
  },
) {
  return {
    id,
    fileId,
    range: {
      kind: "text",
      startLine: startOffset + 1,
      endLine: endOffset + 1,
      startOffset,
      endOffset,
    },
    content: { kind: "text", text },
    metadata: {
      kind: "code",
      symbolType,
      symbolName,
      scope,
      nodeType: `${symbolType}_declaration`,
      signature: `${symbolType} ${symbolName}`,
      doc: null,
      modifiers: [],
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

function createRankedFixture(
  entities,
  files,
  { fragmentByEntityId = new Map(), rankedFragments } = {},
) {
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
    const ranked = rankedFragments
      ? rankedFragments.map((rankedFragment) => ({
          item: entities.find(
            (item) => item.id === (rankedFragment.group ?? rankedFragment.id),
          ),
          fragment: rankedFragment,
        }))
      : entities.map((item) => ({
          item,
          fragment: fragmentByEntityId.get(item.id) ?? fragment(item),
        }));

    return ranked
      .filter(({ item }) => item !== undefined)
      .filter(
        ({ item }) => !filter?.fileIds || filter.fileIds.includes(item.fileId),
      )
      .filter(
        ({ item }) => !filter?.groupIds || filter.groupIds.includes(item.id),
      )
      .map(({ item, fragment: rankedFragment }, index) => ({
        fragment: rankedFragment,
        file: files.find((candidate) => candidate.id === item.fileId),
        path,
        score: 1 - index * 0.01,
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
    searchFts: (query, limit, filter) => hits("fts", filter).slice(0, limit),
    searchVector: (queryVector, limit, filter) =>
      hits("vector", filter).slice(0, limit),
    optimize: () => {},
    close: () => {},
  };

  return {
    collection: {
      id: "collection-1",
      name: "docs",
      path: "/tmp/index",
      rootPaths: [{ absolutePath: "/repo", recursive: true }],
      createdTime: 1,
      updatedTime: 1,
    },
    storage,
    embeddingModel: new FakeEmbeddingModel(),
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
      collection: {
        id: "collection-1",
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
  await assert.rejects(searchPlanCollection({ routes: [] }, context), /route/);
  await assert.rejects(
    searchPlanCollection(
      { routes: [{ mode: "unsupported", query: "value" }] },
      context,
    ),
    /unsupported mode/,
  );
  await assert.rejects(
    searchPlanCollection({ routes: [{ mode: "fts", query: " " }] }, context),
    /non-empty query/,
  );
  await assert.rejects(
    searchPlanCollection(
      { routes: [{ mode: "fts", query: "value" }], includePaths: "src" },
      context,
    ),
    /must be arrays/,
  );
  await assert.rejects(
    searchPlanCollection(
      { routes: [{ mode: "fts", query: "value" }], excludePaths: [1] },
      context,
    ),
    /contain strings/,
  );
  await assert.rejects(
    searchPlanCollection(
      { routes: [{ mode: "fts", query: "value" }], modifiedAfter: -1 },
      context,
    ),
    /non-negative/,
  );
  await assert.rejects(
    searchPlanCollection(
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
    searchPlanCollection(
      { routes: [{ mode: "vector", query: "value" }] },
      { ...context, embeddingModel: undefined },
    ),
    /requires an embedding model/,
  );
});

test("hybrid search filters, deduplicates, fuses, traces, prefers symbols, and tracks hidden hits", async () => {
  const fixture = createFixture();
  const result = await searchPlanCollection(
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
  const result = await searchPlanCollection(
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
  const result = await searchPlanCollection(
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

test("parent-child families keep the container, compact evidence, and backfill the global limit", async () => {
  const childMajorOutline = "primary source\n".repeat(200);
  const files = [
    file("family-file", "src/family.ts"),
    file("other-file", "src/other.ts"),
  ];
  const entities = [
    rangedEntity("child-primary", "family-file", "primary", {
      startOffset: 100,
      endOffset: 200,
      scope: "Container",
      text: childMajorOutline,
    }),
    rangedEntity("parent", "family-file", "Container", {
      startOffset: 0,
      endOffset: 1000,
      symbolType: "class",
      text: "class Container outline",
    }),
    rangedEntity("child-overlap", "family-file", "overlap", {
      startOffset: 110,
      endOffset: 190,
      scope: "Container",
      text: "overlapping source",
    }),
    rangedEntity("child-secondary", "family-file", "secondary", {
      startOffset: 300,
      endOffset: 400,
      scope: "Container",
      text: "secondary source",
    }),
    rangedEntity("unrelated-one", "family-file", "unrelatedOne", {
      startOffset: 1100,
      endOffset: 1200,
      text: "unrelated one",
    }),
    rangedEntity("unrelated-two", "other-file", "unrelatedTwo", {
      startOffset: 0,
      endOffset: 100,
      text: "unrelated two",
    }),
  ];
  const parentWindow = {
    ...fragment(entities[1], { id: "parent-window", group: "parent" }),
    range: {
      kind: "text",
      startLine: 501,
      endLine: 551,
      startOffset: 500,
      endOffset: 550,
    },
    content: { kind: "text", text: "parent source window" },
  };
  const context = createRankedFixture(entities, files, {
    fragmentByEntityId: new Map([["parent", parentWindow]]),
  });
  const result = await searchPlanCollection(
    {
      routes: [
        { mode: "fts", query: "primary query" },
        { mode: "fts", query: "secondary query" },
      ],
      limit: 3,
      trace: true,
    },
    context,
  );

  assert.deepEqual(
    result.hits.map((hit) => hit.entity.id),
    ["child-primary", "unrelated-one", "unrelated-two"],
  );
  assert.deepEqual(
    result.hits.map((hit) => hit.rank),
    [1, 2, 3],
  );
  assert.equal(result.hits[0].score, 2 / 61);
  assert.deepEqual(
    result.hits[0].evidence.map((evidence) => evidence.content.text),
    [childMajorOutline, "parent source window"],
  );
  assert.deepEqual(
    result.hits[0].evidence.map((evidence) => evidence.isEntity),
    [true, false],
  );
  assert.deepEqual(
    result.hits[0].evidence.map((evidence) => evidence.publicEntityId),
    ["child-primary", "parent"],
  );
  assert.equal(result.hits[0].family.root.id, "parent");
  assert.deepEqual(
    result.hits[0].family.members.map((member) => member.entityId),
    ["child-primary", "parent", "child-overlap", "child-secondary"],
  );
  assert.equal(result.hits[0].trace.fusion.rank, 1);
  assert.deepEqual(result.hits[0].trace.compact.originalRanks, [1, 2, 3, 4]);
  assert.deepEqual(
    result.hits[0].trace.compact.suppressed.map((item) => item.entityId),
    ["parent", "child-overlap", "child-secondary"],
  );
  assert.equal(result.hits[1].trace.fusion.rank, 5);
  assert.equal(Object.hasOwn(result.hits[1], "family"), false);
  assert.ok(result.timings.some((entry) => entry.name === "compact"));

  const withoutTrace = await searchPlanCollection(
    {
      routes: [{ mode: "fts", query: "query" }],
      limit: 1,
    },
    context,
  );
  assert.equal(withoutTrace.hits[0].trace, undefined);
  assert.equal(withoutTrace.hits[0].family.root.id, "parent");
});

test("family source windows outrank overlapping child major outlines", async () => {
  const files = [file("family-file", "src/family.ts")];
  const child = rangedEntity("child", "family-file", "method", {
    startOffset: 100,
    endOffset: 900,
    scope: "Container",
    text: "method outline",
  });
  const parent = rangedEntity("parent", "family-file", "Container", {
    startOffset: 0,
    endOffset: 1000,
    symbolType: "class",
    text: "class outline",
  });
  const childWindow = {
    ...fragment(child, { id: "child-window", group: "child" }),
    range: {
      kind: "text",
      startLine: 20,
      endLine: 30,
      startOffset: 200,
      endOffset: 300,
    },
    content: { kind: "text", text: "return actualSource();" },
  };
  const result = await searchPlanCollection(
    {
      routes: [{ mode: "fts", query: "actual source" }],
      limit: 1,
      trace: true,
    },
    createRankedFixture([child, parent], files, {
      rankedFragments: [fragment(child), childWindow, fragment(parent)],
    }),
  );

  assert.equal(result.hits[0].entity.id, "child");
  assert.equal(result.hits[0].family.root.id, "parent");
  assert.equal(result.hits[0].evidence.length, 1);
  assert.equal(
    result.hits[0].evidence[0].content.text,
    "return actualSource();",
  );
  assert.equal(result.hits[0].evidence[0].rank, 2);
  assert.equal(result.hits[0].evidence[0].publicEntityId, "child");
  assert.equal(result.hits[0].evidence[0].isEntity, false);
});

test("complete representative major source survives lower-ranked sibling windows", async () => {
  const files = [file("family-file", "src/family.ts")];
  const representative = rangedEntity(
    "representative",
    "family-file",
    "smallMethod",
    {
      startOffset: 100,
      endOffset: 105,
      scope: "Container",
      text: [
        "function smallMethod() {",
        "  const value = compute();",
        "  if (value) {",
        "    return value;",
        "  }",
        "}",
      ].join("\n"),
    },
  );
  const siblingOne = rangedEntity("sibling-one", "family-file", "siblingOne", {
    startOffset: 300,
    endOffset: 400,
    scope: "Container",
  });
  const siblingTwo = rangedEntity("sibling-two", "family-file", "siblingTwo", {
    startOffset: 500,
    endOffset: 600,
    scope: "Container",
  });
  const parent = rangedEntity("parent", "family-file", "Container", {
    startOffset: 0,
    endOffset: 1000,
    symbolType: "class",
    text: "class outline",
  });
  const siblingOneWindow = {
    ...fragment(siblingOne, {
      id: "sibling-one-window",
      group: "sibling-one",
    }),
    range: {
      kind: "text",
      startLine: 320,
      endLine: 330,
      startOffset: 320,
      endOffset: 330,
    },
    content: { kind: "text", text: "sibling one source" },
  };
  const siblingTwoWindow = {
    ...fragment(siblingTwo, {
      id: "sibling-two-window",
      group: "sibling-two",
    }),
    range: {
      kind: "text",
      startLine: 520,
      endLine: 530,
      startOffset: 520,
      endOffset: 530,
    },
    content: { kind: "text", text: "sibling two source" },
  };
  const result = await searchPlanCollection(
    {
      routes: [{ mode: "fts", query: "small method" }],
      limit: 1,
      trace: true,
    },
    createRankedFixture(
      [representative, siblingOne, siblingTwo, parent],
      files,
      {
        rankedFragments: [
          fragment(representative),
          siblingOneWindow,
          siblingTwoWindow,
          fragment(parent),
        ],
      },
    ),
  );

  assert.equal(result.hits[0].entity.id, "representative");
  assert.equal(result.hits[0].family.root.id, "parent");
  assert.deepEqual(
    result.hits[0].evidence.map((evidence) => evidence.publicEntityId),
    ["representative", "sibling-one"],
  );
  assert.deepEqual(
    result.hits[0].evidence.map((evidence) => evidence.isEntity),
    [true, false],
  );
});

test("family detection rejects cross-file, adjacent, scope-mismatched, and module relationships", async () => {
  const files = [file("file-a", "src/a.ts"), file("file-b", "src/b.ts")];
  const entities = [
    rangedEntity("parent", "file-a", "Container", {
      startOffset: 0,
      endOffset: 1000,
      symbolType: "class",
    }),
    rangedEntity("cross-file", "file-b", "crossFile", {
      startOffset: 100,
      endOffset: 200,
      scope: "Container",
    }),
    rangedEntity("adjacent", "file-a", "adjacent", {
      startOffset: 1000,
      endOffset: 1100,
      scope: "Container",
    }),
    rangedEntity("scope-mismatch", "file-a", "scopeMismatch", {
      startOffset: 300,
      endOffset: 400,
      scope: "OtherContainer",
    }),
    rangedEntity("module", "file-a", "Package", {
      startOffset: 1200,
      endOffset: 1800,
      symbolType: "module",
    }),
    rangedEntity("module-child", "file-a", "moduleChild", {
      startOffset: 1300,
      endOffset: 1400,
      scope: "Package",
    }),
  ];
  const result = await searchPlanCollection(
    {
      routes: [{ mode: "fts", query: "query" }],
      limit: entities.length,
      trace: true,
    },
    createRankedFixture(entities, files),
  );

  assert.deepEqual(
    result.hits.map((hit) => hit.entity.id),
    entities.map((item) => item.id),
  );
  assert.equal(result.hits[0].evidence[0].isEntity, true);
  assert.equal(result.hits[0].evidence[0].publicEntityId, "parent");
  assert.ok(result.hits.every((hit) => !Object.hasOwn(hit, "family")));
  assert.ok(result.hits.every((hit) => hit.trace.compact === undefined));
});

test("visible tracked children stay exact and independent from structural families", async () => {
  const files = [file("family-file", "src/family.ts")];
  const entities = [
    rangedEntity("tracked-child", "family-file", "trackedMethod", {
      startOffset: 100,
      endOffset: 200,
      scope: "Container",
    }),
    rangedEntity("parent", "family-file", "Container", {
      startOffset: 0,
      endOffset: 1000,
      symbolType: "class",
    }),
    rangedEntity("sibling", "family-file", "sibling", {
      startOffset: 300,
      endOffset: 400,
      scope: "Container",
    }),
  ];
  const result = await searchPlanCollection(
    {
      routes: [{ mode: "fts", query: "query" }],
      limit: 1,
      trace: true,
      trackEntityId: "tracked-child",
    },
    createRankedFixture(entities, files),
  );

  assert.deepEqual(
    result.hits.map((hit) => hit.entity.id),
    ["tracked-child"],
  );
  assert.equal(result.trackedHit?.entity.id, "tracked-child");
  assert.equal(result.trackedHit?.rank, 1);
  assert.equal(Object.hasOwn(result.trackedHit, "family"), false);
  assert.equal(result.trackedHit?.evidence.length, 1);
  assert.equal(result.trackedHit?.evidence[0].isEntity, true);
  assert.equal(result.trackedHit?.evidence[0].publicEntityId, "tracked-child");
  assert.equal(result.trackedHit?.trace.fusion.rank, 1);
  assert.equal(result.trackedHit?.trace.final.returnedByLimit, true);
});

test("hidden tracked children append exactly once and never enter parent evidence", async () => {
  const files = [file("family-file", "src/family.ts")];
  const entities = [
    rangedEntity("sibling", "family-file", "sibling", {
      startOffset: 300,
      endOffset: 400,
      scope: "Container",
    }),
    rangedEntity("parent", "family-file", "Container", {
      startOffset: 0,
      endOffset: 1000,
      symbolType: "class",
    }),
    rangedEntity("tracked-child", "family-file", "trackedMethod", {
      startOffset: 100,
      endOffset: 200,
      scope: "Container",
    }),
  ];
  const result = await searchPlanCollection(
    {
      routes: [{ mode: "fts", query: "query" }],
      limit: 1,
      trace: true,
      trackEntityId: "tracked-child",
    },
    createRankedFixture(entities, files),
  );

  assert.deepEqual(
    result.hits.map((hit) => hit.entity.id),
    ["sibling", "tracked-child"],
  );
  assert.equal(result.hits[0].family.root.id, "parent");
  assert.deepEqual(
    result.hits[0].family.members.map((member) => member.entityId),
    ["sibling", "parent"],
  );
  assert.deepEqual(
    result.hits[0].evidence.map((evidence) => evidence.publicEntityId),
    ["sibling"],
  );
  assert.equal(result.trackedHit?.entity.id, "tracked-child");
  assert.equal(result.trackedHit?.rank, 2);
  assert.equal(Object.hasOwn(result.trackedHit, "family"), false);
  assert.equal(result.trackedHit?.evidence.length, 1);
  assert.equal(result.trackedHit?.evidence[0].isEntity, true);
  assert.equal(result.trackedHit?.trace.final.returnedByLimit, false);
});
