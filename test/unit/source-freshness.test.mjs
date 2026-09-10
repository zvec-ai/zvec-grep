import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import fs from "node:fs";
import { mkdir, stat, writeFile } from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { join } from "node:path";
import test from "node:test";
import {
  inspectIndexedSource,
  MAX_CHANGED_SOURCE_HASH_BYTES,
  verifyWorkspaceSourceFreshness,
} from "../../dist/engine/source-freshness.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

const hash = (text) => createHash("sha256").update(text).digest("hex");

test("indexed freshness captures immutable indexed identity and hashes later same-size drift versions", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-source-evidence-");
  const file = await sourceFile(root, "source.ts", "old source");
  const fresh = inspectIndexedSource("workspace-old", file);
  assert.equal(fresh.status, "fresh");
  assert.equal(fresh.indexed.workspaceIndexId, "workspace-old");
  assert.equal(fresh.indexed.contentHash, hash("old source"));
  file.indexStatus.indexedTime = 999;
  assert.equal(fresh.indexed.indexedTime, 123);

  await writeFile(file.absolutePath, "new source longer");
  const first = inspectIndexedSource("workspace-old", file);
  assert.equal(first.invalidation.reason, "size_mismatch");
  assert.equal(first.invalidation.observedHash, hash("new source longer"));
  assert.equal(first.invalidation.observedSizeBytes, 17);
  await writeFile(file.absolutePath, "new source newest");
  const second = inspectIndexedSource("workspace-old", file);
  assert.equal(second.invalidation.reason, "size_mismatch");
  assert.equal(
    first.invalidation.observedSizeBytes,
    second.invalidation.observedSizeBytes,
  );
  assert.notEqual(
    first.invalidation.observedHash,
    second.invalidation.observedHash,
  );
  assert.deepEqual(first.invalidation.indexed, second.invalidation.indexed);

  await writeFile(file.absolutePath, "new source");
  assert.equal(
    inspectIndexedSource("workspace-old", file).invalidation.reason,
    "hash_mismatch",
  );
  const rebuilt = inspectIndexedSource("workspace-new", file);
  assert.equal(rebuilt.invalidation.indexed.workspaceIndexId, "workspace-new");
});

test("source evidence distinguishes missing, non-file, unreadable and unverified records", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-source-reasons-");
  const file = await sourceFile(root, "source.ts", "current source");
  const directory = join(root, "directory.ts");
  await mkdir(directory);
  assert.equal(
    inspectIndexedSource("workspace", {
      ...file,
      absolutePath: join(root, "missing.ts"),
    }).invalidation.reason,
    "missing",
  );
  assert.equal(
    inspectIndexedSource("workspace", { ...file, absolutePath: directory })
      .invalidation.reason,
    "not_file",
  );
  for (const override of [
    { contentHash: undefined },
    { indexStatus: { indexedTime: null, entityCount: 0 } },
    { indexStatus: { indexedTime: 123, entityCount: 0, error: "failed" } },
  ]) {
    const result = inspectIndexedSource("workspace", { ...file, ...override });
    assert.equal(result.invalidation.reason, "unverified");
    assert.equal(result.invalidation.observedHash, hash("current source"));
  }
  const original = fs.readFileSync;
  const mocked = t.mock.method(fs, "readFileSync", (...args) => {
    if (String(args[0]) === file.absolutePath) {
      throw Object.assign(new Error("private read failure"), {
        code: "EACCES",
      });
    }
    return original(...args);
  });
  syncBuiltinESMExports();
  try {
    const result = inspectIndexedSource("workspace", file);
    assert.equal(result.invalidation.reason, "unreadable");
    assert.doesNotMatch(JSON.stringify(result), /private read failure/);
  } finally {
    mocked.mock.restore();
    syncBuiltinESMExports();
  }
});

test("an obviously oversized changed source is invalidated without reading its bytes", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-source-size-cap-");
  const file = await sourceFile(root, "source.ts", "small indexed source");
  const originalStat = fs.statSync;
  const originalRead = fs.readFileSync;
  let reads = 0;
  const mockedStat = t.mock.method(fs, "statSync", (...args) =>
    String(args[0]) === file.absolutePath
      ? { isFile: () => true, size: MAX_CHANGED_SOURCE_HASH_BYTES + 1 }
      : originalStat(...args),
  );
  const mockedRead = t.mock.method(fs, "readFileSync", (...args) => {
    if (String(args[0]) === file.absolutePath) reads++;
    return originalRead(...args);
  });
  syncBuiltinESMExports();
  try {
    const inspected = inspectIndexedSource("workspace", file);
    assert.equal(inspected.status, "possibly_stale");
    assert.equal(inspected.invalidation.reason, "size_mismatch");
    assert.equal(
      inspected.invalidation.observedSizeBytes,
      MAX_CHANGED_SOURCE_HASH_BYTES + 1,
    );
    assert.equal(inspected.invalidation.observedHash, undefined);
    assert.equal(reads, 0);
  } finally {
    mockedStat.mock.restore();
    mockedRead.mock.restore();
    syncBuiltinESMExports();
  }
});

test("post-index proof reads only exact current records and snapshots requested paths", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-source-proof-");
  const first = await sourceFile(root, "first.ts", "first");
  const second = await sourceFile(root, "second.ts", "second");
  const missing = join(root, "missing.ts");
  const returned = new Map([
    [first.absolutePath, first],
    [second.absolutePath, second],
  ]);
  const paths = [
    first.absolutePath,
    second.absolutePath,
    first.absolutePath,
    missing,
  ];
  const calls = [];
  const proof = await verifyWorkspaceSourceFreshness(
    workspace(root),
    {
      getFileByPath(path) {
        calls.push(path);
        paths.splice(0, paths.length, join(root, "mutated.ts"));
        return returned.get(path) ?? null;
      },
    },
    paths,
  );
  assert.deepEqual(calls, [first.absolutePath, second.absolutePath, missing]);
  assert.equal(proof.workspaceIndexId, "workspace");
  assert.deepEqual(
    proof.paths.map((entry) => entry.status),
    ["fresh", "fresh", "absent"],
  );
  assert.equal(proof.paths[0].indexed.contentHash, hash("first"));
  first.contentHash = "mutated";
  assert.equal(proof.paths[0].indexed.contentHash, hash("first"));
});

test("a missing record is not proof of absent source, and an unreadable path is not absent", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-source-absence-");
  const file = await sourceFile(root, "present.ts", "present");
  const directory = join(root, "directory.ts");
  await mkdir(directory);
  const denied = join(root, "denied.ts");
  const outside = join(root, "..", "outside-source-proof.ts");
  const original = fs.statSync;
  const mocked = t.mock.method(fs, "statSync", (...args) => {
    const path = String(args[0]);
    if (path === denied)
      throw Object.assign(new Error("denied"), { code: "EACCES" });
    assert.notEqual(
      path,
      outside,
      "out-of-scope proof must not read arbitrary source",
    );
    return original(...args);
  });
  syncBuiltinESMExports();
  try {
    const proof = await verifyWorkspaceSourceFreshness(
      workspace(root),
      {
        getFileByPath: () => null,
      },
      [file.absolutePath, denied, outside, directory],
    );
    assert.deepEqual(
      proof.paths.map(({ status, reason }) => ({ status, reason })),
      [
        { status: "unverified", reason: "not_indexed" },
        { status: "unverified", reason: "unreadable" },
        { status: "unverified", reason: "out_of_scope" },
        { status: "unverified", reason: "not_indexed" },
      ],
    );
  } finally {
    mocked.mock.restore();
    syncBuiltinESMExports();
  }
});

test("proof reports stale committed records and preserves cancellation identity", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-source-proof-cancel-");
  const file = await sourceFile(root, "source.ts", "old");
  await writeFile(file.absolutePath, "new");
  let lookups = 0;
  const storage = {
    getFileByPath() {
      lookups++;
      return file;
    },
  };
  const proof = await verifyWorkspaceSourceFreshness(workspace(root), storage, [
    file.absolutePath,
  ]);
  assert.equal(proof.paths[0].status, "unverified");
  assert.equal(proof.paths[0].invalidation.indexed.contentHash, hash("old"));
  assert.equal(proof.paths[0].invalidation.observedHash, hash("new"));
  const reason = { caller: "cancelled" };
  await assert.rejects(
    verifyWorkspaceSourceFreshness(
      workspace(root),
      storage,
      [file.absolutePath],
      AbortSignal.abort(reason),
    ),
    (error) => error === reason,
  );
  assert.equal(lookups, 1);
  await assert.rejects(
    verifyWorkspaceSourceFreshness(workspace(root), storage, ["relative.ts"]),
    (error) => error.code === "ZVEC_GREP.ENGINE.FRESHNESS.INVALID_PATH",
  );
});

async function sourceFile(root, name, text) {
  const absolutePath = join(root, name);
  await writeFile(absolutePath, text);
  const info = await stat(absolutePath);
  return {
    id: name,
    absolutePath,
    relativePath: name,
    rootPath: root,
    sizeBytes: Buffer.byteLength(text),
    lastModifiedTime: Math.trunc(info.mtimeMs),
    contentHash: hash(text),
    kind: "code",
    format: "typescript",
    indexStatus: { indexedTime: 123, entityCount: 1 },
  };
}

function workspace(root) {
  return {
    id: "workspace",
    rootPaths: [{ absolutePath: root, recursive: true }],
  };
}
