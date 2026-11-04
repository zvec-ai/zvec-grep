import assert from "node:assert/strict";
import { mkdtemp, rm, readdir, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  ZVecCollectionSchema,
  ZVecCreateAndOpen,
  ZVecDataType,
  ZVecIndexType,
} from "@zvec/zvec";
import { createWorkspaceIndexStorage } from "../../dist/engine/storage/index.js";
import { queryFileMetadataDocs } from "../../dist/engine/storage/zvec.js";

test("DiskANN vectors persist, respect metadata filters, and follow replacement and deletion", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "zvec-grep-diskann-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const options = storageOptions(parent);
  const write = (storage, id, vector) => {
    const file = fileInfo(id, parent, `${id}.ts`);
    storage.replaceFile(file, [
      {
        fragment: {
          id,
          fileId: file.id,
          range: {
            kind: "text",
            startLine: 1,
            endLine: 1,
            startOffset: 0,
            endOffset: 6,
          },
          content: { kind: "text", text: "orchard" },
        },
        vector,
      },
    ]);
  };
  const original = createWorkspaceIndexStorage(options);
  try {
    write(original, "a", [1, 0]);
    write(original, "b", [0, 1]);
    assert.equal(original.searchVector([1, 0], 1)[0].fragment.id, "a");
    assert.equal(
      original.searchVector([1, 0], 1, { fileIds: ["b".repeat(64)] })[0]
        .fragment.id,
      "b",
    );
    assert.equal(original.searchVector([1, 0], 1, { fileIds: [] }).length, 0);
    assert.equal(original.searchFts("orchard", 10).length, 2);
    write(original, "a", [-1, 0]);
    assert.equal(original.searchVector([1, 0], 1)[0].fragment.id, "b");
    await original.finalizeWrites();
    assert.equal(
      original.collection.schema.vector("embedding").indexParams.indexType,
      ZVecIndexType.DISKANN,
    );
    assert.equal(
      original.collection.schema.vector("embedding").dataType,
      ZVecDataType.VECTOR_FP16,
    );
    original.deleteFile("b".repeat(64));
    await original.finalizeWrites();
  } finally {
    original.close();
  }
  const reader = createWorkspaceIndexStorage({ ...options, readOnly: true });
  try {
    assert.deepEqual(
      reader.searchVector([-1, 0], 10).map((hit) => hit.fragment.id),
      ["a"],
    );
    assert.equal(reader.searchFts("orchard", 10).length, 1);
  } finally {
    reader.close();
  }
  // Recreate the previous metadata-only collection in this temporary test index.
  const indexPath = join(options.storagePath, "index.zvec");
  await rm(indexPath, { recursive: true });
  const legacy = ZVecCreateAndOpen(
    indexPath,
    new ZVecCollectionSchema({
      name: "legacy",
      fields: [{ name: "document_id", dataType: ZVecDataType.STRING }],
    }),
  );
  legacy.closeSync();
  for (const readOnly of [true, false]) {
    assert.throws(
      () => createWorkspaceIndexStorage({ ...options, readOnly }),
      /rebuild/,
    );
  }
});

test("zvec builds on-disk DiskANN and PQ segments and searches them after reopen", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "zvec-grep-native-diskann-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const options = storageOptions(parent);
  options.embedding.dimension = 32;
  const file = fileInfo("a", parent, "vectors.ts");
  let seed = 17;
  const entries = Array.from({ length: 1024 }, (_, i) => ({
    fragment: {
      id: `vector-${i}`,
      fileId: file.id,
      range: {
        kind: "text",
        startLine: 1,
        endLine: 1,
        startOffset: 0,
        endOffset: 6,
      },
      content: { kind: "text", text: "orchard" },
    },
    vector: Array.from({ length: 32 }, () => {
      seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
      return seed / 2 ** 32 - 0.5;
    }),
  }));
  const writer = createWorkspaceIndexStorage(options);
  try {
    writer.replaceFile(file, entries);
    await writer.finalizeWrites();
    assert.equal(writer.collection.stats.indexCompleteness.embedding, 1);
  } finally {
    writer.close();
  }
  const root = join(options.storagePath, "index.zvec");
  const files = (await readdir(root, { recursive: true })).filter((path) =>
    path.endsWith(".proxima"),
  );
  const segments = await Promise.all(
    files.map((path) => readFile(join(root, path))),
  );
  assert.ok(
    segments.some((bytes) => bytes.includes(Buffer.from("diskann.pq_data"))),
    "must materialize zvec DiskANN PQ data, not only declare the schema",
  );
  assert.ok(
    segments.some((bytes) => bytes.includes(Buffer.from("diskann.vector"))),
  );
  const reader = createWorkspaceIndexStorage({
    storagePath: options.storagePath,
    readOnly: true,
  });
  try {
    for (const entry of entries.slice(0, 20)) {
      const hits = reader.searchVector(entry.vector, 10, {
        fileIds: [file.id],
      });
      assert.equal(hits[0].fragment.id, entry.fragment.id);
      assert.ok(hits[0].score > 0.99);
    }
  } finally {
    reader.close();
  }
});

test("zvec vector scores preserve larger-is-better semantics for each metric", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "zvec-grep-diskann-scores-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  for (const [metric, expected] of [
    ["cosine", 1],
    ["dot_product", 2],
    ["euclidean", -1],
  ]) {
    const options = storageOptions(join(parent, metric));
    options.embedding.metric = metric;
    const storage = createWorkspaceIndexStorage(options);
    try {
      const file = fileInfo("a", parent, "a.ts");
      storage.replaceFile(
        file,
        [
          [2, 0],
          [0, 1],
        ].map((vector, i) => ({
          fragment: {
            id: String(i),
            fileId: file.id,
            range: {
              kind: "text",
              startLine: 1,
              endLine: 1,
              startOffset: 0,
              endOffset: 1,
            },
            content: { kind: "text", text: "x" },
          },
          vector,
        })),
      );
      await storage.finalizeWrites();
      const hits = storage.searchVector([1, 0], 2);
      assert.equal(hits[0].fragment.id, "0");
      assert.equal(hits[0].score, expected);
      assert.ok(hits[0].score > hits[1].score);
      assert.deepEqual(storage.searchVector([1, 0], 0), []);
    } finally {
      storage.close();
    }
  }
});

function doc(id) {
  return {
    id,
    fields: { file_id: id },
    vectors: {},
    score: 0,
  };
}

test("file metadata queries partition beyond zvec's top-k limit", () => {
  const documents = [
    doc(`${"0".repeat(64)}`),
    doc(`0${"f".repeat(63)}`),
    doc(`1${"0".repeat(63)}`),
    doc(`a${"5".repeat(63)}`),
    doc(`f${"f".repeat(63)}`),
    doc(`b${"0".repeat(63)}`),
  ];
  const queries = [];
  const collection = {
    stats: { docCount: documents.length, indexCompleteness: {} },
    querySync(query) {
      queries.push(query);
      const lower = /file_id >= '([^']+)'/.exec(query.filter)?.[1];
      const upper = /file_id < '([^']+)'/.exec(query.filter)?.[1];
      return documents
        .filter((item) => lower === undefined || item.id >= lower)
        .filter((item) => upper === undefined || item.id < upper)
        .slice(0, query.topk);
    },
  };

  const result = queryFileMetadataDocs(collection, 2);

  assert.deepEqual(
    result.map((item) => item.id).sort(),
    documents.map((item) => item.id).sort(),
  );
  assert.ok(queries.length > 16);
  assert.ok(queries.every((query) => query.topk <= 2));
});

test("file metadata queries use one request below zvec's top-k limit", () => {
  const queries = [];
  const collection = {
    stats: { docCount: 2, indexCompleteness: {} },
    querySync(query) {
      queries.push(query);
      return [doc(`0${"0".repeat(63)}`), doc(`f${"f".repeat(63)}`)];
    },
  };

  const result = queryFileMetadataDocs(collection, 2);

  assert.equal(result.length, 2);
  assert.equal(queries.length, 1);
  assert.equal(queries[0].topk, 2);
});

test("file metadata partitions use zvec string range semantics", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "zvec-grep-file-meta-range-"));
  const collection = ZVecCreateAndOpen(
    join(parent, "collection"),
    new ZVecCollectionSchema({
      name: "file_metadata_range",
      fields: [{ name: "file_id", dataType: ZVecDataType.STRING }],
    }),
  );
  t.after(async () => {
    collection.closeSync();
    await rm(parent, { recursive: true, force: true });
  });

  const documents = [
    doc(`${"0".repeat(64)}`),
    doc(`0${"f".repeat(63)}`),
    doc(`1${"0".repeat(63)}`),
    doc(`a${"5".repeat(63)}`),
    doc(`f${"f".repeat(63)}`),
    doc(`b${"0".repeat(63)}`),
  ];
  collection.insertSync(
    documents.map((item) => ({ id: item.id, fields: item.fields })),
  );

  const result = queryFileMetadataDocs(collection, 2);

  assert.deepEqual(
    result.map((item) => item.id).sort(),
    documents.map((item) => item.id).sort(),
  );
});

test("file metadata supports one batched path-prefix lookup", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "zvec-grep-file-prefixes-"));
  const root = join(parent, "repo");
  const storage = createWorkspaceIndexStorage({
    storagePath: join(parent, "storage"),
    readOnly: false,
    embedding: {
      provider: "local",
      model: "test",
      dimension: 2,
      metric: "cosine",
    },
  });
  t.after(async () => {
    storage.close();
    await rm(parent, { recursive: true, force: true });
  });
  const files = [
    fileInfo("a", root, "src/a.ts"),
    fileInfo("b", root, "src/nested/b.ts"),
    fileInfo("c", root, "docs/c.md"),
  ];
  for (const file of files) storage.replaceFile(file, []);

  const matches = storage.listFilesByPathPrefixes([
    join(root, "src"),
    join(root, "docs", "c.md"),
  ]);

  assert.deepEqual(matches.map((file) => file.relativePath).sort(), [
    "docs/c.md",
    "src/a.ts",
    "src/nested/b.ts",
  ]);
});

function fileInfo(id, root, relativePath) {
  return {
    id: id.repeat(64),
    absolutePath: join(root, relativePath),
    relativePath,
    rootPath: root,
    sizeBytes: 1,
    lastModifiedTime: 1,
    kind: relativePath.endsWith(".md") ? "markdown" : "code",
    format: relativePath.endsWith(".md") ? "markdown" : "typescript",
  };
}

function storageOptions(parent) {
  return {
    storagePath: join(parent, "storage"),
    readOnly: false,
    embedding: {
      provider: "local",
      model: "test",
      dimension: 2,
      metric: "cosine",
    },
  };
}

test("failed optimization still allows close and reopen in the same process", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "zvec-grep-close-failure-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const options = storageOptions(parent);
  const storage = createWorkspaceIndexStorage(options);
  const failure = new Error(
    "FtsRocksdbReducer: source postings is not BitPacked. field=text",
  );
  // Inject the native failure while retaining real collections and their locks.
  storage.needsOptimize = true;
  const nativeCollection = storage.collection;
  const retry = t.mock.fn(() => {
    throw failure;
  });
  storage.collection = {
    optimize: async () => {
      throw failure;
    },
    optimizeSync: retry,
    closeSync: () => nativeCollection.closeSync(),
  };
  try {
    await assert.rejects(
      storage.finalizeWrites(),
      (error) => error === failure,
    );
    assert.doesNotThrow(() => storage.close());
    assert.equal(retry.mock.callCount(), 0);
    const reopened = createWorkspaceIndexStorage(options);
    reopened.close();
  } finally {
    closeIfOpen(storage.files.collection);
    closeIfOpen(storage.collection);
  }
});

test("metadata close failure does not skip closing the entity collection", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "zvec-grep-close-both-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const options = storageOptions(parent);
  const storage = createWorkspaceIndexStorage(options);
  const originalClose = storage.files.close.bind(storage.files);
  const failure = new Error("metadata close failed");
  t.mock.method(storage.files, "close", () => {
    originalClose();
    throw failure;
  });
  try {
    assert.throws(
      () => storage.close(),
      (error) => error === failure,
    );
    const reopened = createWorkspaceIndexStorage(options);
    reopened.close();
  } finally {
    closeIfOpen(storage.collection);
  }
});

test("failed storage initialization releases the metadata collection", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "zvec-grep-open-failure-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const options = storageOptions(parent);
  const initial = createWorkspaceIndexStorage(options);
  const prototype = Object.getPrototypeOf(initial.files);
  initial.close();
  const failure = new Error("metadata load failed");
  const openedCollections = [];
  const list = t.mock.method(prototype, "list", function () {
    openedCollections.push(this.collection);
    throw failure;
  });
  try {
    assert.throws(
      () => createWorkspaceIndexStorage(options),
      (error) => error === failure,
    );
    list.mock.restore();
    const reopened = createWorkspaceIndexStorage(options);
    reopened.close();
  } finally {
    list.mock.restore();
    for (const collection of openedCollections) closeIfOpen(collection);
  }
});

function closeIfOpen(collection) {
  try {
    collection.closeSync();
  } catch (error) {
    if (error.code !== "ZVEC_FAILED_PRECONDITION") throw error;
  }
}

test("metadata optimization failure does not block close or lose written metadata", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "zvec-grep-metadata-optimize-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const options = storageOptions(parent);
  const storage = createWorkspaceIndexStorage(options);
  const file = fileInfo("a", parent, "a.ts");
  storage.replaceFile(file, []);
  const nativeCollection = storage.files.collection;
  const failure = new Error("metadata optimize failed");
  const retry = t.mock.fn(() => {
    throw failure;
  });
  storage.files.collection = {
    optimize: async () => {
      throw failure;
    },
    optimizeSync: retry,
    closeSync: () => nativeCollection.closeSync(),
  };
  try {
    await assert.rejects(
      storage.finalizeWrites(),
      (error) => error === failure,
    );
    storage.close();
    assert.equal(retry.mock.callCount(), 0);
    const reopened = createWorkspaceIndexStorage(options);
    try {
      assert.equal(reopened.getFileByPath(file.absolutePath).id, file.id);
    } finally {
      reopened.close();
    }
  } finally {
    closeIfOpen(nativeCollection);
    closeIfOpen(storage.collection);
  }
});

test("unfinalized FTS writes survive close and can be optimized after reopen", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "zvec-grep-unfinalized-fts-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const options = storageOptions(parent);
  const writeFile = (storage, id) => {
    const file = fileInfo(id, parent, `${id}.ts`);
    storage.replaceFile(file, [
      {
        fragment: {
          id,
          fileId: file.id,
          range: {
            kind: "text",
            startLine: 1,
            endLine: 1,
            startOffset: 0,
            endOffset: 6,
          },
          content: { kind: "text", text: "shared" },
        },
        vector: [1, 0],
      },
    ]);
  };
  const original = createWorkspaceIndexStorage(options);
  try {
    writeFile(original, "a");
  } finally {
    original.close();
  }
  const recovered = createWorkspaceIndexStorage(options);
  try {
    assert.equal(recovered.searchFts("shared", 10).length, 1);
    writeFile(recovered, "b");
    await recovered.finalizeWrites();
  } finally {
    recovered.close();
  }
  const reopened = createWorkspaceIndexStorage(options);
  try {
    assert.equal(reopened.searchFts("shared", 10).length, 2);
    assert.equal(reopened.listFiles().length, 2);
  } finally {
    reopened.close();
  }
});

test("finalize waits for metadata optimization and does not repeat it after success", async (t) => {
  const parent = await mkdtemp(join(tmpdir(), "zvec-grep-await-optimize-"));
  t.after(() => rm(parent, { recursive: true, force: true }));
  const storage = createWorkspaceIndexStorage(storageOptions(parent));
  storage.replaceFile(fileInfo("a", parent, "a.ts"), []);
  const nativeCollection = storage.files.collection;
  const entered = Promise.withResolvers();
  const release = Promise.withResolvers();
  let calls = 0;
  let finished = false;
  storage.files.collection = {
    async optimize() {
      calls++;
      entered.resolve();
      await release.promise;
      await nativeCollection.optimize();
    },
    closeSync: () => nativeCollection.closeSync(),
  };
  const pending = storage.finalizeWrites().then(() => {
    finished = true;
  });
  try {
    await entered.promise;
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(finished, false);
    release.resolve();
    await pending;
    assert.equal(finished, true);
    await storage.finalizeWrites();
    assert.equal(calls, 1);
  } finally {
    release.resolve();
    await pending;
    storage.close();
  }
});
