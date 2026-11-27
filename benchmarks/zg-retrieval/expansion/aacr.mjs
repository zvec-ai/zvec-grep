// SPDX-License-Identifier: Apache-2.0
import assert from "node:assert/strict";
import { lstat } from "node:fs/promises";
import { join } from "node:path";
import { prepareCorpus } from "../core/corpus.mjs";
import { run, sha256 } from "../core/io.mjs";
import { indexSelectionArguments } from "../engines/zg/run.mjs";
export const aacrCorpusId = (repo) => `${repo.repository}@${repo.commit}`;
export function validateAacrLock(lock) {
  assert.equal(lock.suite, "aacr6");
  assert.deepEqual(
    lock.tasks.map((t) => t.id),
    ["Q02", "Q03", "Q04", "Q05", "Q07", "Q09"],
  );
  assert.equal(lock.repositories.length, 3);
  assert.equal(new Set(lock.repositories.map(aacrCorpusId)).size, 3);
  for (const repo of lock.repositories) {
    assert.match(repo.repository, /^[\w.-]+\/[\w.-]+$/);
    assert.match(repo.commit, /^[a-f0-9]{40}$/);
    assert.match(repo.tree_oid, /^[a-f0-9]{40}$/);
  }
  for (const task of lock.tasks) {
    assert.equal(sha256(task.query), task.query_sha256);
    assert.ok(
      lock.repositories.some(
        (r) => r.repository === task.repository && r.commit === task.revision,
      ),
    );
    assert.equal(
      task.evaluation_role ?? "semantic",
      task.id === "Q09" ? "lookup_control" : "semantic",
    );
  }
  assert.equal(lock.index_selection.content, "code");
  assert.equal(lock.index_selection.max_file_size_bytes, 1000000);
  assert.ok(lock.index_selection.code_extensions.length > 0);
  assert.ok(
    lock.index_selection.code_extensions.every((x) =>
      /^\.[a-z0-9_-]+$/.test(x),
    ),
  );
  assert.ok(!lock.index_selection.code_extensions.includes(".md"));
}
export async function prepareAacrRepository(pilot, repo, directory) {
  const root = await prepareCorpus(
    { ...repo, url: `https://github.com/${repo.repository}.git` },
    join(directory, "corpus"),
  );
  try {
    await lstat(join(root, ".zvec-grep"));
    assert.fail("AACR requires a fresh index");
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
  assert.equal(
    (await run("git", ["-C", root, "rev-parse", "HEAD^{tree}"])).stdout.trim(),
    repo.tree_oid,
  );
  return {
    id: aacrCorpusId(repo),
    root,
    sourceCommit: repo.commit,
    captureSourceInventory: true,
    tasks: pilot.lock.tasks
      .filter(
        (t) => t.repository === repo.repository && t.revision === repo.commit,
      )
      .map((t) => ({
        ...t,
        targets: pilot.gold.tasks[t.id].files.map(({ path }) => ({ path })),
      })),
    indexArguments: indexSelectionArguments({
      index_selection: pilot.lock.index_selection,
    }),
  };
}
