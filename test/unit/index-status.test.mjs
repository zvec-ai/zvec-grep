import assert from "node:assert/strict";
import test from "node:test";
import {
  indexCompletionForJob,
  indexCompletionFromStatus,
  indexStatusNeedsRefresh,
  mergeIndexCompletion,
} from "../../dist/engine/index-status.js";

test("index completion uses the configured scope and advances with successful work", () => {
  const status = {
    filesScanned: 5,
    filesUnchanged: 3,
    filesAdded: 1,
    filesModified: 1,
    filesDeleted: 2,
    filesPending: 0,
    filesFailed: 0,
  };
  const completion = indexCompletionFromStatus(status);

  assert.equal(indexStatusNeedsRefresh(status), true);
  assert.deepEqual(completion, { completed: 3, total: 5 });
  assert.deepEqual(
    mergeIndexCompletion(completion, {
      phase: "indexing",
      filesIndexed: 2,
      filesFailed: 1,
      filesTotal: 2,
    }),
    { completed: 4, total: 5 },
  );
  assert.deepEqual(
    mergeIndexCompletion(undefined, {
      phase: "indexing",
      filesIndexed: 4,
      filesFailed: 1,
      filesTotal: 10,
    }),
    { completed: 3, total: 10 },
  );

  const progress = {
    phase: "indexing",
    filesIndexed: 2,
    filesFailed: 1,
    filesTotal: 2,
  };
  assert.deepEqual(indexCompletionForJob(completion, "running", progress), {
    completed: 4,
    total: 5,
  });
  for (const state of ["queued", "succeeded", "failed", "cancelled"]) {
    assert.deepEqual(indexCompletionForJob(completion, state, progress), {
      completed: 3,
      total: 5,
    });
  }
});
