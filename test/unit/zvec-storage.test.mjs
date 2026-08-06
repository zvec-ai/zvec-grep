import assert from "node:assert/strict";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  ZVecCollectionSchema,
  ZVecCreateAndOpen,
  ZVecDataType,
} from "@zvec/zvec";
import { queryFileMetadataDocs } from "../../dist/engine/storage/zvec.js";

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
