// SPDX-License-Identifier: Apache-2.0
import assert from "node:assert/strict";
import test from "node:test";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  loadPilot,
  pilotCorpusIds,
  targetsForTask,
} from "../expansion/datasets.mjs";
import { prepareAacrRepository, validateAacrLock } from "../expansion/aacr.mjs";
import {
  validatePilotGold,
  summarizePilotEntity,
  verifyPilotGoldSources,
} from "../expansion/gold.mjs";
import {
  searchArguments,
  summarizePilotRows,
  summarizePilotBreakdown,
} from "../expansion/run.mjs";
import { summarizeRepeatedQuality } from "../metrics/repetitions.mjs";
import { buildCombined } from "../expansion/combined.mjs";
import {
  PILOT_PROTOCOL,
  PILOT_PREVIEW,
  PILOT_QA_SCORING,
} from "../expansion/config.mjs";
import { scoreFileRetrieval } from "../metrics/files.mjs";
import { scoreNdcg } from "../metrics/ndcg.mjs";
import { run, sha256 } from "../core/io.mjs";

const data = new URL("../expansion/data/aacr.json", import.meta.url);
const gold = new URL("../expansion/gold/aacr.json", import.meta.url);

test("AACR reuses pilot contracts for six inputs and their reviewed targets", async () => {
  const pilot = await loadPilot("aacr");
  assert.deepEqual(
    pilot.lock.tasks.map((t) => t.id),
    ["Q02", "Q03", "Q04", "Q05", "Q07", "Q09"],
  );
  assert.equal(pilotCorpusIds(pilot).length, 3);
  assert.deepEqual(pilot.modes, ["hybrid", "fts", "vector"]);
  // Six queries, three modes and five calls per question and mode.
  assert.equal(pilot.modes.length * pilot.lock.tasks.length * 5, 90);
  assert.deepEqual(
    Object.values(pilot.gold.tasks).map((t) => t.targets.length),
    [6, 4, 2, 1, 4, 1],
  );
  const review = JSON.parse(
    await readFile(
      new URL("../expansion/gold/aacr-review.json", import.meta.url),
    ),
  );
  assert.match(review.evidence_archive.sha256, /^[a-f0-9]{64}$/);
  for (const task of pilot.lock.tasks) {
    assert.deepEqual(
      review.queries[task.id].target_ids,
      pilot.gold.tasks[task.id].targets.map((t) => t.source_id),
    );
    assert.deepEqual(
      targetsForTask(pilot, task).map((t) => t.path),
      pilot.gold.tasks[task.id].files.map((t) => t.path),
    );
    for (const mode of pilot.modes) {
      const args = searchArguments(task, "/corpus", mode);
      assert.equal(mode === "hybrid" ? args.query : args[mode][0], task.query);
      assert.equal(args.limit, 10);
      assert.equal(args.preview, "full");
    }
  }
  const bytes = await readFile(data);
  const labels = JSON.parse(await readFile(gold));
  validatePilotGold(labels, pilot.lock, bytes);
  const changed = structuredClone(pilot.lock);
  changed.tasks[0].query += " changed";
  assert.throws(() => validateAacrLock(changed));
  const wrongRole = structuredClone(pilot.lock);
  wrongRole.tasks.at(-1).evaluation_role = "semantic";
  assert.throws(() => validateAacrLock(wrongRole));
  labels.source_lock_sha256 = "0".repeat(64);
  assert.throws(
    () => validatePilotGold(labels, pilot.lock, bytes),
    /source lock/,
  );
});

test("AACR upstream license texts match their checksums and pinned commits", async () => {
  const pilot = await loadPilot("aacr");
  const { sources } = JSON.parse(
    await readFile(
      new URL("../expansion/gold/aacr-source-licenses.json", import.meta.url),
    ),
  );
  const pinned = new Map(
    pilot.lock.repositories.map((repo) => [repo.repository, repo.commit]),
  );
  assert.ok(sources.length > 0);
  for (const source of sources) {
    assert.equal(pinned.get(source.repository), source.commit);
    assert.match(source.path ?? "", /^[\w./-]+$/);
    assert.equal(sha256(source.text), source.sha256, source.path);
  }
  // Every indexed repository ships at least one recorded license or notice.
  assert.deepEqual(
    [...new Set(sources.map((source) => source.repository))].sort(),
    [...pinned.keys()].sort(),
  );
});

test("AACR rejects wrong trees, changed files and stale indexes", async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "aacr-source-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const root = join(directory, "corpus", "example__repo");
  await mkdir(root, { recursive: true });
  await run("git", ["init", "--quiet", root]);
  await writeFile(join(root, "sample.rs"), "fn sample() {}\n");
  await run("git", ["-C", root, "add", "sample.rs"]);
  await run("git", [
    "-C",
    root,
    "-c",
    "user.name=Fixture",
    "-c",
    "user.email=fixture@example.invalid",
    "-c",
    "core.hooksPath=/dev/null",
    "commit",
    "--quiet",
    "-m",
    "fixture",
  ]);
  const repo = {
    repository: "example/repo",
    commit: (await run("git", ["-C", root, "rev-parse", "HEAD"])).stdout.trim(),
    tree_oid: (
      await run("git", ["-C", root, "rev-parse", "HEAD^{tree}"])
    ).stdout.trim(),
  };
  const pilot = await loadPilot("aacr");
  pilot.lock.tasks = [
    { id: "fixture", repository: repo.repository, revision: repo.commit },
  ];
  pilot.gold.tasks = { fixture: { files: [{ path: "sample.rs" }] } };
  const group = await prepareAacrRepository(pilot, repo, directory);
  assert.equal(group.tasks.length, 1);
  assert.ok(group.indexArguments.includes("--iglob"));
  await assert.rejects(
    prepareAacrRepository(
      pilot,
      { ...repo, tree_oid: "0".repeat(40) },
      directory,
    ),
  );
  await writeFile(join(root, "sample.rs"), "fn changed() {}\n");
  await assert.rejects(
    prepareAacrRepository(pilot, repo, directory),
    /pristine/,
  );
  await writeFile(join(root, "sample.rs"), "fn sample() {}\n");
  await mkdir(join(root, ".zvec-grep"));
  await assert.rejects(
    prepareAacrRepository(pilot, repo, directory),
    /fresh index/,
  );
});

test("shared verification rejects changed evidence bytes", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "aacr-evidence-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const bytes = Buffer.from("fn sample() {}\n");
  await writeFile(join(root, "sample.rs"), bytes);
  const labels = {
    tasks: {
      fixture: {
        files: [
          { path: "sample.rs", bytes: bytes.length, sha256: sha256(bytes) },
        ],
        targets: [
          {
            source_id: "sample",
            path: "sample.rs",
            start_line: 1,
            end_line: 1,
            start_byte: 0,
            end_byte: bytes.length,
            sha256: sha256(bytes),
          },
        ],
      },
    },
  };
  const group = { root, tasks: [{ id: "fixture" }] };
  await verifyPilotGoldSources(labels, group);
  await writeFile(join(root, "sample.rs"), "fn changed() {}\n");
  await assert.rejects(verifyPilotGoldSources(labels, group), /Changed Gold/);
});

test("lookup control cannot lift semantic quality or alter native ranks", async () => {
  const pilot = await loadPilot("aacr");
  const rows = pilot.lock.tasks.map((task) => {
    const entry = pilot.gold.tasks[task.id];
    const targets = targetsForTask(pilot, task);
    const items =
      task.evaluation_role === "lookup_control"
        ? [{ path: targets[0].path, rank: 1 }]
        : [];
    const calls = Array.from({ length: 5 }, (_, i) => ({
      repetition: i + 1,
      status: "success",
      latency_ms: 10,
      items,
    }));
    return {
      task_id: task.id,
      mode: "hybrid",
      evaluation_role: task.evaluation_role,
      status: "success",
      output_bytes: 10,
      calls,
      quality_mean: summarizeRepeatedQuality(calls, targets, {
        includeFileRecall: true,
      }),
      qa_quality_mean: summarizePilotEntity(calls, entry),
    };
  });
  const result = summarizePilotRows(rows, ["hybrid"], 6)[0];
  assert.equal(result.completed, 6);
  assert.equal(result.metrics.file_hit_at_10, 0);
  assert.equal(result.lookup_control.metrics.file_hit_at_10, 1);
  assert.equal(result.measurements.latency_sample_count, 30);
  const entry = pilot.gold.tasks.Q09;
  const target = entry.targets[0];
  const items = [
    ...Array.from({ length: 10 }, (_, i) => ({
      path: "other.rs",
      rank: i + 1,
    })),
    ...target.evidence_sets[0].spans.map((range, index) => ({
      path: target.path,
      rank: 11 + index,
      start_byte: range.start_byte,
      end_byte: range.end_byte,
    })),
  ];
  const calls = Array.from({ length: 5 }, (_, i) => ({
    repetition: i + 1,
    status: "success",
    items,
  }));
  assert.ok(summarizePilotEntity(calls, entry));
  assert.equal(calls[0].items[10].rank, 11);
});

test("combined reports bind the control role and Gold identity to frozen inputs", async () => {
  const pilot = await loadPilot("aacr");
  const candidateCommit = "a".repeat(40);
  const rows = pilot.lock.tasks.flatMap((task) =>
    pilot.modes.map((mode) => {
      const targets = targetsForTask(pilot, task);
      const calls = Array.from({ length: 5 }, (_, i) => ({
        repetition: i + 1,
        status: "success",
        latency_ms: 1,
        items: [],
      }));
      return {
        task_id: task.id,
        query: task.query,
        evaluation_role: task.evaluation_role,
        mode,
        targets,
        calls,
        status: "success",
        group_integrity: "verified",
        output_bytes: 0,
        items: [],
        file: scoreFileRetrieval([], targets),
        ndcg: scoreNdcg([], targets),
        quality_mean: summarizeRepeatedQuality(calls, targets, {
          includeFileRecall: true,
        }),
        qa_quality_mean: summarizePilotEntity(calls, pilot.gold.tasks[task.id]),
      };
    }),
  );
  const report = {
    schema_version: 3,
    protocol: PILOT_PROTOCOL,
    preview: PILOT_PREVIEW,
    quality_aggregation: "mean_of_five",
    qa_gold: pilot.gold.identity,
    qa_scoring: PILOT_QA_SCORING,
    suite: pilot.lock.suite,
    model: pilot.model,
    candidate_commit: candidateCommit,
    status: "success",
    rows,
    failures: [],
    summary: summarizePilotRows(rows, pilot.modes, 6),
    breakdown: summarizePilotBreakdown(pilot, rows),
  };
  const check = async () =>
    (await buildCombined({ aacr: report, candidateCommit })).pilots.aacr;
  assert.equal((await check()).status, "success");
  rows.find((row) => row.task_id === "Q09").evaluation_role = "semantic";
  assert.match((await check()).error, /Changed query role/);
  rows.find((row) => row.task_id === "Q09").evaluation_role = "lookup_control";
  report.qa_gold = { ...pilot.gold.identity, sha256: "0".repeat(64) };
  assert.match((await check()).error, /Gold changed/);
});
