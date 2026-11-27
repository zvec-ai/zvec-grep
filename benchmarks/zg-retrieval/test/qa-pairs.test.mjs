import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);
const readJson = async (path) =>
  JSON.parse(await readFile(new URL(path, root), "utf8"));
const sha256 = (value) => createHash("sha256").update(value).digest("hex");
const pairs = await readJson("gold/qa-pairs-v1.json");
const lock = await readJson("data/source.lock.json");
const queryRows = (await readFile(new URL("data/queries.jsonl", root), "utf8"))
  .trim()
  .split("\n")
  .map((line) => JSON.parse(line));
const shaPattern = /^[a-f0-9]{64}$/;

test("QA pairs preserve all twenty original questions and pinned source commits", () => {
  assert.equal(pairs.schema_version, 1);
  assert.equal(pairs.id, "sweqa20-qa-pairs-v1");
  assert.deepEqual(
    Object.keys(pairs.tasks),
    lock.tasks.map((task) => task.task_id),
  );
  assert.equal(Object.keys(pairs.tasks).length, 20);
  for (const frozen of lock.tasks) {
    const actual = pairs.tasks[frozen.task_id];
    const query = queryRows.find((row) => row.task_id === frozen.task_id);
    assert.equal(actual.query, frozen.query);
    assert.equal(actual.query, query.query);
    assert.equal(
      actual.query_sha256,
      sha256(Buffer.from(actual.query, "utf8")),
    );
    assert.equal(actual.query_sha256, frozen.query_sha256);
    assert.equal(actual.repository, frozen.repository);
    assert.equal(actual.repository_commit, frozen.repository_commit);
    assert.equal(actual.category, frozen.category);
    assert.match(actual.source_archive_sha256, shaPattern);
  }
});

test("historical positive review does not pass the formal completeness gate", () => {
  assert.equal(pairs.status, "diagnostic_only");
  const eligible = [];
  const disputed = [];
  for (const [taskId, row] of Object.entries(pairs.tasks)) {
    assert.equal(row.scoring_eligible, false, taskId);
    assert.equal(row.review.completeness, "not_reviewed", taskId);
    assert.ok(row.exclusion_reasons.includes("completeness_review_required"));
    if (row.diagnostic_eligible) {
      eligible.push(taskId);
      assert.equal(row.review.correctness, "reviewed");
      assert.equal(row.interpretations, undefined);
    } else {
      disputed.push(taskId);
      assert.equal(row.review.correctness, "disputed");
      assert.ok(row.exclusion_reasons.includes("ambiguous_question"));
      assert.ok(row.interpretations.groups.length >= 2);
      const ids = new Set(
        [...row.file_targets, ...row.entity_targets].map((target) => target.id),
      );
      for (const id of [
        ...row.interpretations.shared_target_ids,
        ...row.interpretations.groups.flatMap((group) => group.target_ids),
      ]) {
        assert.ok(
          ids.has(id),
          `${taskId}: interpretation references missing target ${id}`,
        );
      }
    }
  }
  assert.equal(eligible.length, 17);
  assert.deepEqual(disputed.sort(), [
    "django:32",
    "streamlink:14",
    "streamlink:43",
  ]);
});

test("fixed-source declarations and anchors agree with their exact definition bytes", () => {
  let files = 0;
  let entities = 0;
  for (const [taskId, row] of Object.entries(pairs.tasks)) {
    const fileTargets = new Map(
      row.file_targets.map((target) => [target.id, target]),
    );
    assert.equal(fileTargets.size, row.file_targets.length);
    assert.equal(
      new Set(row.entity_targets.map((target) => target.id)).size,
      row.entity_targets.length,
    );
    for (const target of row.file_targets) {
      files += 1;
      assert.ok(
        !target.path.startsWith("/") && !target.path.split("/").includes(".."),
      );
      assert.match(target.file_sha256, shaPattern);
      const anchor = Buffer.from(target.anchor.text, "utf8");
      assert.equal(sha256(anchor), target.anchor.sha256);
      assert.equal(
        anchor.length,
        target.anchor.end_byte - target.anchor.start_byte,
      );
    }
    for (const target of row.entity_targets) {
      entities += 1;
      const parent = fileTargets.get(target.file_target_id);
      assert.ok(parent, `${taskId}: missing parent file`);
      assert.equal(parent.path, target.path);
      assert.equal(parent.file_sha256, target.file_sha256);
      assert.ok(target.symbol.length && target.kind.length);
      const definition = Buffer.from(target.definition_text, "utf8");
      assert.equal(sha256(definition), target.definition_sha256);
      assert.equal(definition.length, target.end_byte - target.start_byte);
      const lines = target.definition_text.match(/[^\n]*\n|[^\n]+$/g) ?? [];
      assert.equal(lines.length, target.end_line - target.start_line + 1);
      assert.ok(target.declaration_start_line >= target.start_line);
      assert.ok(target.declaration_end_line <= target.end_line);
      assert.equal(
        target.declaration_text,
        lines
          .slice(
            target.declaration_start_line - target.start_line,
            target.declaration_end_line - target.start_line + 1,
          )
          .join(""),
      );
      for (const anchor of target.anchors) {
        assert.ok(
          anchor.start_byte >= target.start_byte &&
            anchor.end_byte <= target.end_byte,
        );
        const bytes = definition.subarray(
          anchor.start_byte - target.start_byte,
          anchor.end_byte - target.start_byte,
        );
        assert.equal(bytes.toString("utf8"), anchor.text);
        assert.equal(sha256(bytes), anchor.sha256);
      }
    }
  }
  assert.equal(files, 65);
  assert.equal(entities, 114);
});

test("source attribution and provenance are portable", () => {
  const repositories = new Set(lock.repositories.map((row) => row.repository));
  assert.deepEqual(new Set(Object.keys(pairs.source_licenses)), repositories);
  for (const [repository, source] of Object.entries(pairs.source_licenses)) {
    const frozen = lock.repositories.find(
      (row) => row.repository === repository,
    );
    assert.equal(source.commit, frozen.commit);
    assert.ok(source.files.length);
    for (const license of source.files) {
      assert.equal(sha256(Buffer.from(license.text, "utf8")), license.sha256);
      assert.ok(
        license.url.startsWith(
          `https://github.com/${repository}/blob/${source.commit}/`,
        ),
      );
    }
  }
  const stripSourceText = (key, value) =>
    ["text", "definition_text", "declaration_text"].includes(key)
      ? undefined
      : value;
  const metadata = JSON.stringify(pairs, stripSourceText);
  assert.ok(!/\/Users\/|\/home\/|\/private\//.test(metadata));
  assert.ok(!/[\u4e00-\u9fff]/.test(metadata));
  for (const hash of Object.values(pairs.provenance.source_files))
    assert.match(hash, shaPattern);
});
