import assert from "node:assert/strict";
import { readFile, realpath } from "node:fs/promises";
import { dirname, join, resolve, isAbsolute } from "node:path";
import { fileURLToPath } from "node:url";
import { readJson, sha256, objectHash, inside, fileHash } from "./io.mjs";
import { embeddingModel } from "./embedding.mjs";

export const suiteDirectory = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "..",
);

export async function loadSuite({
  embedding = process.env.RETRIEVAL_EMBEDDING ?? "local",
} = {}) {
  const lock = await readJson(join(suiteDirectory, "data/source.lock.json"));
  const protocol = await readJson(
    join(suiteDirectory, "configs/protocol.json"),
  );
  assert.deepEqual(protocol.modes, ["hybrid", "fts", "vector"]);
  assert.equal(protocol.preview, "mcp-default");
  assert.equal(protocol.request.preferSymbol, false);
  assert.equal(protocol.repetitions, 5);
  assert.equal(protocol.quality_repetition, 5);
  assert.equal(protocol.limit, 10);
  assert.equal(protocol.call_order, "mode-task-repetition");
  assert.equal(protocol.index_embedding_concurrency, 4);
  assert.equal(protocol.query_concurrency, 4);
  protocol.model = embeddingModel(protocol.model, embedding);
  assert.equal(
    lock.tasks.length,
    20,
    "the frozen suite must contain 20 original questions",
  );
  assert.equal(new Set(lock.tasks.map((task) => task.task_id)).size, 20);
  assert.equal(lock.repositories.length, 11);
  const queries = (
    await readFile(join(suiteDirectory, "data/queries.jsonl"), "utf8")
  )
    .trimEnd()
    .split("\n")
    .map(JSON.parse);
  const gold = {};
  for (const [index, task] of lock.tasks.entries()) {
    assert.equal(
      sha256(task.query),
      task.query_sha256,
      `${task.task_id}: query hash mismatch`,
    );
    assert.equal(queries[index].task_id, task.task_id);
    assert.equal(
      queries[index].query,
      task.query,
      `${task.task_id}: rewritten query`,
    );
    assert.equal(
      task.repository_commit,
      lock.repositories.find((repo) => repo.repository === task.repository)
        ?.commit,
    );
    const entry = await readJson(
      join(suiteDirectory, "gold/v1", `${task.task_slug}.json`),
    );
    validateGold(task, entry);
    gold[task.task_id] = entry;
  }
  assert.equal(queries.length, lock.tasks.length);
  const fileGold = await readJson(join(suiteDirectory, "gold/files-v1.json"));
  assert.equal(fileGold.schema_version, 1);
  assert.equal(fileGold.id, "sweqa20-accepted-files-v1");
  assert.equal(
    fileGold.source_gold_sha256,
    objectHash(gold),
    "stale file-label projection",
  );
  assert.equal(
    fileGold.metric_source_commit,
    "0051e000fcaac69a9c5d081ebbc8d4cb8508160b",
  );
  assert.deepEqual(
    Object.keys(fileGold.tasks).sort(),
    lock.tasks.map((task) => task.task_id).sort(),
  );
  for (const task of lock.tasks) {
    const projected = fileGold.tasks[task.task_id];
    assert.equal(projected.repository, task.repository);
    assert.equal(projected.language, "python");
    assert.equal(projected.query_sha256, task.query_sha256);
    const paths = [
      ...new Set(
        gold[task.task_id].targets
          .filter((target) => target.role === "accepted")
          .map((target) => target.path),
      ),
    ];
    assert.deepEqual(
      projected.targets,
      paths.map((path) => ({ path })),
      "file labels must be the frozen accepted-file projection",
    );
  }
  return {
    lock,
    protocol,
    gold,
    file_gold: fileGold.tasks,
    identity: {
      source: objectHash(lock),
      protocol: objectHash(protocol),
      gold: objectHash(gold),
      file_gold: objectHash(fileGold),
    },
  };
}

export function validateGold(task, gold) {
  assert.equal(gold.schema_version, 1);
  assert.equal(gold.task_id, task.task_id);
  assert.equal(gold.question_sha256, task.query_sha256);
  assert.equal(gold.repository, task.repository);
  assert.equal(gold.repository_commit, task.repository_commit);
  assert.match(gold.gold_version, /^sweqa20-entry-v/);
  assert.ok(
    ["reviewed", "unknown", "disputed"].includes(gold.status),
    `${task.task_id}: unreviewed gold`,
  );
  if (gold.status === "reviewed") {
    assert.ok(
      gold.review?.proposer &&
        gold.review?.reviewer &&
        gold.review.proposer !== gold.review.reviewer,
      `${task.task_id}: independent review missing`,
    );
    assert.ok(gold.targets.some((target) => target.role === "accepted"));
  }
  const ids = new Set();
  for (const target of gold.targets) {
    assert.ok(target.id && !ids.has(target.id));
    ids.add(target.id);
    assert.ok(
      !isAbsolute(target.path) && !target.path.split(/[\\/]/).includes(".."),
    );
    assert.ok(["symbol", "code_span"].includes(target.kind));
    assert.ok(["accepted", "bridge"].includes(target.role));
    assert.match(target.source_sha256, /^[a-f0-9]{64}$/);
    assert.ok(target.relevance_reason && target.anchors.length > 0);
    for (const anchor of target.anchors) {
      assert.ok(Number.isInteger(anchor.start_line) && anchor.start_line > 0);
      assert.ok(
        Number.isInteger(anchor.end_line) &&
          anchor.end_line >= anchor.start_line,
      );
      assert.equal(
        anchor.text.split("\n").length,
        anchor.end_line - anchor.start_line + 1,
      );
      assert.equal(sha256(anchor.text), anchor.sha256);
    }
  }
  assert.equal(typeof gold.ndcg?.enabled, "boolean");
  if (gold.ndcg.enabled) {
    assert.ok(
      gold.ndcg.groups.length >= 2,
      "nDCG subset requires multiple complementary evidence groups",
    );
    assert.equal(
      new Set(gold.ndcg.groups.map((group) => group.id)).size,
      gold.ndcg.groups.length,
    );
    const seen = new Set();
    for (const group of gold.ndcg.groups) {
      assert.ok(group.target_ids.length > 0);
      for (const id of group.target_ids) {
        assert.ok(
          gold.targets.some(
            (target) => target.id === id && target.role === "accepted",
          ),
        );
        assert.ok(
          !seen.has(id),
          "an entry cannot represent two complementary groups",
        );
        seen.add(id);
      }
    }
  }
}

export async function validateGoldSources(root, tasks, gold) {
  for (const task of tasks) {
    for (const target of gold[task.task_id].targets) {
      const path = join(root, target.path);
      assert.ok(
        inside(await realpath(root), await realpath(path)),
        "gold source escapes corpus",
      );
      assert.equal(
        await fileHash(path),
        target.source_sha256,
        `${task.task_id}: stale gold file ${target.path}`,
      );
      const lines = (await readFile(path, "utf8")).split(/\r?\n/);
      for (const anchor of target.anchors) {
        assert.equal(
          lines.slice(anchor.start_line - 1, anchor.end_line).join("\n"),
          anchor.text,
          `${task.task_id}: stale source anchor ${target.id}`,
        );
      }
    }
  }
}
