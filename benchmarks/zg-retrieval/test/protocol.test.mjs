import assert from "node:assert/strict";
import {
  cp,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";
import test from "node:test";
import {
  fileHash,
  loadSuite,
  objectHash,
  readJson,
  repositorySlug,
  sha256,
  suiteDirectory,
  validateGold,
  validateGoldSources,
  writeJson,
} from "../core/lib.mjs";
import {
  callPlan,
  requestArguments,
  indexSelectionArguments,
  auditIndexSelection,
} from "../engines/zg/run.mjs";
import { aggregate } from "../engines/zg/report.mjs";

const suite = await loadSuite();
const clone = (value) => structuredClone(value);
const fixedTask = suite.lock.tasks.find((task) => task.task_id === "sympy:38");

async function temporary(t) {
  const directory = await mkdtemp(join(tmpdir(), "zg-retrieval-protocol-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  return directory;
}

async function isolatedSuite(t) {
  const directory = await temporary(t);
  for (const name of ["core", "data", "configs", "gold"])
    await cp(join(suiteDirectory, name), join(directory, name), {
      recursive: true,
    });
  const isolated = await import(
    pathToFileURL(join(directory, "core/lib.mjs")).href
  );
  return { directory, load: isolated.loadSuite };
}

test("the locked suite preserves all original questions, category balance and independently reviewed labels", async () => {
  const data = join(
    dirname(suiteDirectory),
    "swe-qa-bench/zg_bench/swe_qa/data",
  );
  const selectionBytes = await readFile(join(data, "selection.json"));
  const referenceBytes = await readFile(join(data, "references.json"));
  assert.equal(sha256(selectionBytes), suite.lock.source.selection_sha256);
  assert.equal(sha256(referenceBytes), suite.lock.source.references_sha256);
  const originals = JSON.parse(selectionBytes).tasks;
  assert.equal(suite.lock.tasks.length, 20);
  assert.equal(
    new Set(suite.lock.tasks.map((task) => task.repository)).size,
    11,
  );
  assert.equal(suite.lock.repositories.length, 11);
  for (const category of ["what", "where", "how", "why"])
    assert.equal(
      suite.lock.tasks.filter((task) => task.category === category).length,
      5,
    );
  for (const task of suite.lock.tasks) {
    const original = originals.find((entry) => entry.task_id === task.task_id);
    assert.ok(original);
    assert.equal(task.query, original.question);
    assert.equal(sha256(task.query), original.question_hash);
    const gold = suite.gold[task.task_id];
    assert.equal(gold.status, "reviewed");
    assert.notEqual(gold.review.proposer, gold.review.reviewer);
  }
  // This punctuation is part of the real source question, not an interchangeable ASCII hyphen.
  assert.ok(
    suite.lock.tasks
      .find((task) => task.task_id === "conan:27")
      .query.includes("built\u2011in"),
  );
  assert.equal(
    Object.values(suite.gold).reduce(
      (sum, gold) => sum + gold.targets.length,
      0,
    ),
    60,
  );
  assert.equal(
    Object.values(suite.gold).filter((gold) => gold.ndcg.enabled).length,
    12,
  );
});

test("loadSuite rejects a rewritten Unicode query even when all task IDs are unchanged", async (t) => {
  const fixture = await isolatedSuite(t);
  const path = join(fixture.directory, "data/queries.jsonl");
  const queries = (await readFile(path, "utf8"))
    .trimEnd()
    .split("\n")
    .map(JSON.parse);
  const query = queries.find((entry) => entry.task_id === "conan:27");
  query.query = query.query.replace("\u2011", "-");
  await writeFile(
    path,
    queries.map((entry) => JSON.stringify(entry)).join("\n") + "\n",
  );
  await assert.rejects(fixture.load, /rewritten query/);
});

test("loadSuite rejects stale question hashes and repository revision drift", async (t) => {
  const fixture = await isolatedSuite(t);
  const path = join(fixture.directory, "data/source.lock.json");
  const lock = await readJson(path);
  const changedQuestion = clone(lock);
  changedQuestion.tasks[0].query += " extra locator";
  await writeJson(path, changedQuestion);
  await assert.rejects(fixture.load, /query hash mismatch/);
  const changedRevision = clone(lock);
  changedRevision.tasks[0].repository_commit = "0".repeat(40);
  await writeJson(path, changedRevision);
  await assert.rejects(fixture.load, { name: "AssertionError" });
});

test("gold cannot be self-reviewed, reused for another question, or double-count a relevance group", () => {
  const gold = suite.gold[fixedTask.task_id];
  for (const mutate of [
    (entry) => {
      entry.status = "proposed";
    },
    (entry) => {
      entry.review.reviewer = entry.review.proposer;
    },
    (entry) => {
      entry.question_sha256 = "0".repeat(64);
    },
    (entry) => {
      entry.ndcg.groups[1].target_ids.push(entry.ndcg.groups[0].target_ids[0]);
    },
    (entry) => {
      entry.targets[0].path = "../outside.py";
    },
  ]) {
    const changed = clone(gold);
    mutate(changed);
    assert.throws(() => validateGold(fixedTask, changed), {
      name: "AssertionError",
    });
  }
});

async function sourceFixture(t) {
  const directory = await temporary(t);
  const root = join(directory, "corpus");
  await mkdir(root);
  const bytes = Buffer.from("def entry():\r\n    return 1\r\n", "utf8");
  await writeFile(join(root, "entry.py"), bytes);
  const gold = clone(suite.gold[fixedTask.task_id]);
  gold.targets = [
    {
      ...gold.targets[0],
      path: "entry.py",
      symbol: "entry",
      source_sha256: sha256(bytes),
      anchors: [
        {
          start_line: 1,
          end_line: 2,
          text: "def entry():\n    return 1",
          sha256: sha256("def entry():\n    return 1"),
        },
      ],
    },
  ];
  return { directory, root, gold, byTask: { [fixedTask.task_id]: gold } };
}

test("source validation accepts CRLF bytes with LF anchors and detects file or anchor drift separately", async (t) => {
  const fixture = await sourceFixture(t);
  await validateGoldSources(fixture.root, [fixedTask], fixture.byTask);
  const changed = clone(fixture.byTask);
  changed[fixedTask.task_id].targets[0].anchors[0].text =
    "def entry():\n    return 2";
  changed[fixedTask.task_id].targets[0].anchors[0].sha256 = sha256(
    "def entry():\n    return 2",
  );
  await assert.rejects(
    () => validateGoldSources(fixture.root, [fixedTask], changed),
    /stale source anchor/,
  );
  await writeFile(
    join(fixture.root, "entry.py"),
    "def entry():\n    return 2\n",
  );
  await assert.rejects(
    () => validateGoldSources(fixture.root, [fixedTask], fixture.byTask),
    /stale gold file/,
  );
});

test("matching file hashes do not authorize reading a gold target through a corpus-escaping symlink", async (t) => {
  const fixture = await sourceFixture(t);
  const outside = join(fixture.directory, "outside.py");
  await cp(join(fixture.root, "entry.py"), outside);
  await symlink(outside, join(fixture.root, "escape.py"));
  fixture.gold.targets[0].path = "escape.py";
  await assert.rejects(
    () => validateGoldSources(fixture.root, [fixedTask], fixture.byTask),
    /escapes corpus/,
  );
});

test("each mode receives the unmodified original question exactly once, without an extra hybrid query", () => {
  const task = suite.lock.tasks.find((entry) => entry.task_id === "conan:27");
  for (const mode of ["hybrid", "fts", "vector"]) {
    const args = requestArguments(task, "/app", mode, suite.protocol);
    const field = mode === "hybrid" ? "query" : mode;
    assert.deepEqual(
      Object.keys(args).sort(),
      [
        "root",
        field,
        "limit",
        "autoUpdate",
        "freshness",
        "preferSymbol",
      ].sort(),
    );
    assert.deepEqual(
      args[field],
      mode === "hybrid" ? task.query : [task.query],
    );
    assert.equal(args.autoUpdate, false);
    assert.equal(args.freshness, "eventual");
    assert.equal(args.limit, 10);
    assert.equal(Object.hasOwn(args, "preview"), false);
    assert.equal(args.preferSymbol, false);
  }
  assert.throws(
    () => requestArguments(task, "/app", "expanded", suite.protocol),
    /unknown mode/,
  );
  assert.throws(
    () =>
      requestArguments(task, "/app", "hybrid", {
        ...suite.protocol,
        preview: "full",
      }),
    /requires the public MCP default presentation/,
  );
});

test("the fixed plan runs all three modes with five Rust MCP calls per question", () => {
  const tasks = suite.lock.tasks;
  const plan = callPlan(tasks, suite.protocol);
  assert.equal(plan.length, 300);
  assert.deepEqual(
    [...new Set(plan.map((call) => call.mode))],
    ["hybrid", "fts", "vector"],
  );
  assert.ok(plan.slice(0, 100).every((call) => call.mode === "hybrid"));
  for (const mode of suite.protocol.modes) {
    const calls = plan.filter((call) => call.mode === mode);
    assert.equal(calls.filter((call) => call.quality_observation).length, 20);
    for (const [index, task] of tasks.entries()) {
      const repeats = calls.slice(index * 5, (index + 1) * 5);
      assert.ok(
        repeats.every(
          (call) =>
            call.task.task_id === task.task_id &&
            call.preview === "mcp-default",
        ),
      );
      assert.deepEqual(
        repeats.map((call) => call.repetition),
        [1, 2, 3, 4, 5],
      );
      assert.ok(
        repeats.every(
          (call) => call.quality_observation === (call.repetition === 5),
        ),
      );
    }
  }
  assert.throws(() =>
    callPlan(tasks, { ...suite.protocol, modes: ["hybrid"] }),
  );
  assert.throws(() =>
    callPlan(tasks, { ...suite.protocol, modes: ["vector", "fts", "hybrid"] }),
  );
});

test("code-only index arguments cover the frozen code extension set and reject leaked documents or oversized files", () => {
  const args = indexSelectionArguments(suite.protocol);
  assert.deepEqual(args.slice(0, 2), ["--max-filesize", "1000000"]);
  const extensions = [];
  for (let i = 2; i < args.length; i += 2) {
    assert.equal(args[i], "--iglob");
    assert.ok(args[i + 1].length < 1024);
    extensions.push(...args[i + 1].slice(2, -1).split(","));
  }
  assert.deepEqual(extensions, suite.protocol.index_selection.code_extensions);
  assert.ok(extensions.includes(".py"));
  assert.ok(!extensions.includes(".md") && !extensions.includes(".json"));
  const files = [{ relativePath: "source/ENTRY.PY", sizeBytes: 1000000 }];
  assert.equal(auditIndexSelection(files, suite.protocol).verified, true);
  assert.throws(
    () =>
      auditIndexSelection(
        [{ relativePath: "README.md", sizeBytes: 20 }],
        suite.protocol,
      ),
    /non-code extension/,
  );
  assert.throws(
    () =>
      auditIndexSelection(
        [{ relativePath: "entry.py", sizeBytes: 1000001 }],
        suite.protocol,
      ),
    /oversized/,
  );
});

test("the independent file-label projection is frozen and cannot silently inherit a new anchor target", async (t) => {
  assert.equal(
    Object.values(suite.file_gold).reduce(
      (n, entry) => n + entry.targets.length,
      0,
    ),
    39,
  );
  const fixture = await isolatedSuite(t);
  const path = join(fixture.directory, "gold/files-v1.json");
  const labels = await readJson(path);
  labels.tasks[fixedTask.task_id].targets.push({ path: "unreviewed.py" });
  await writeJson(path, labels);
  await assert.rejects(fixture.load, /frozen accepted-file projection/);
});

async function reportFixture(t) {
  const directory = await temporary(t);
  const shard = join(directory, repositorySlug(fixedTask.repository));
  const root = "/app";
  const modes = suite.protocol.modes;
  const plan = callPlan([fixedTask], suite.protocol);
  const manifest = {
    schema_version: 1,
    run_id: "unit-test-product-failure",
    repository: fixedTask.repository,
    repository_commit: fixedTask.repository_commit,
    source_root: root,
    corpus_root: root,
    started_at: "2026-09-17T00:00:00.000Z",
    finished_at: "2026-09-17T00:01:00.000Z",
    suite: suite.identity,
    protocol: suite.protocol,
    package: {
      name: "@zvec/zvec-grep",
      version: "test-only",
      tarball_sha256: "a".repeat(64),
      consumer_lock_sha256: "b".repeat(64),
    },
    candidate_commit: "c".repeat(40),
    model_files_sha256: "d".repeat(64),
    index_content_sha256: "e".repeat(64),
    tasks: [fixedTask.task_id],
    modes,
    preview: suite.protocol.preview,
    planned_calls: plan.length,
    invalid_reasons: [],
    preparation_status: "index_failed",
    preparation_error: "Test fixture: index construction failed.",
  };
  const calls = plan.map((call, index) => ({
    task_id: call.task.task_id,
    mode: call.mode,
    preview: call.preview,
    repetition: call.repetition,
    quality_observation: call.quality_observation,
    session_first_query: index === 0,
    latency_ms: 1,
    request: {
      name: "zvec_grep_search",
      arguments: requestArguments(call.task, root, call.mode, suite.protocol),
    },
    transport_error: null,
    preparation_error: manifest.preparation_error,
    raw_path: `raw/${fixedTask.task_slug}-${call.mode}-${call.preview}-${call.repetition}.json`,
  }));
  for (const call of calls) {
    await writeJson(join(shard, call.raw_path), {
      isError: true,
      content: [
        {
          type: "text",
          text: "Test fixture: product could not deliver an entry.",
        },
      ],
    });
    call.raw_sha256 = await fileHash(join(shard, call.raw_path));
  }
  const save = async () => {
    await writeJson(join(shard, "run.json"), manifest);
    await writeFile(
      join(shard, "requests.jsonl"),
      calls.map((call) => JSON.stringify(call)).join("\n") + "\n",
    );
  };
  await save();
  return {
    directory,
    shard,
    manifest,
    calls,
    save,
    score: () => aggregate(directory, { expectedTasks: [fixedTask.task_id] }),
  };
}

function assertNoHeadline(report) {
  assert.equal(report.integrity_passed, false);
  assert.equal(report.quality_score_valid, false);
  assert.ok(report.integrity_errors.length > 0);
  for (const mode of Object.values(report.modes)) {
    assert.equal(mode.file_retrieval, null);
    assert.equal(mode.ndcg, null);
  }
}

test("complete product-error observations retain a zero score and denominator but fail operational integrity", async (t) => {
  const fixture = await reportFixture(t);
  const report = await fixture.score();
  assert.equal(report.scope, "explicit-subset");
  assert.equal(report.observed_calls, 15);
  assert.equal(
    report.quality_score_valid,
    true,
    JSON.stringify(report.integrity_errors),
  );
  assert.equal(report.integrity_passed, false);
  assert.equal(report.product_error_calls, 15);
  assert.deepEqual(report.integrity_errors, []);
  assert.equal(report.tasks.length, 3);
  assert.equal(report.schema_version, 7);
  assert.equal(report.preview, "mcp-default");
  assert.deepEqual(Object.keys(report.modes), ["hybrid", "fts", "vector"]);
  assert.equal(Object.hasOwn(report, "previews"), false);
  assert.equal(report.tasks[0].repetition, 5);
  assert.equal(report.tasks[0].ranking_repeatable, null);
  assert.equal(report.tasks[0].output_repeatable, null);
  const summary = report.modes.hybrid.file_retrieval;
  assert.equal(summary.planned_tasks, 1);
  assert.equal(summary.scored_tasks, 1);
  for (const metric of ["hit_at_1", "hit_at_5", "hit_at_10", "mrr_at_10"])
    assert.equal(summary[metric], 0);
  assert.equal(report.modes.hybrid.ndcg.repository_macro.ndcg_at_10, 0);
  assert.equal(Object.hasOwn(report.modes.hybrid, "summary"), false);
  assert.equal(Object.hasOwn(report.tasks[0], "first_hit_rank"), false);
  assert.equal(Object.hasOwn(report.tasks[0], "target_matches"), false);
  assert.match(
    await readFile(join(fixture.directory, "report.md"), "utf8"),
    /fail operational integrity/,
  );
});

test("isolated public-format failures preserve other arms' diagnostic scores", async (t) => {
  const fixture = await reportFixture(t);
  for (const call of fixture.calls.filter((call) => call.mode === "hybrid")) {
    await writeJson(join(fixture.shard, call.raw_path), {
      isError: false,
      content: [{ type: "text", text: "malformed public search output" }],
    });
    call.raw_sha256 = await fileHash(join(fixture.shard, call.raw_path));
  }
  await fixture.save();
  const report = await fixture.score();
  assert.equal(report.integrity_passed, false);
  assert.equal(report.quality_score_valid, false);
  assert.deepEqual(report.integrity_errors, [
    "one or more observations are experimentally invalid",
  ]);
  assert.equal(report.modes.hybrid.file_retrieval, null);
  assert.equal(report.modes.fts.file_retrieval.scored_tasks, 1);
  assert.equal(report.modes.vector.file_retrieval.scored_tasks, 1);
  assert.match(report.tasks[0].invalid_reason, /^format_unknown: /);
  const markdown = await readFile(join(fixture.directory, "report.md"), "utf8");
  assert.match(markdown, /Invalid task\/mode observations/);
  assert.match(markdown, /zg-fts \| 1\/1 questions/);
});

test("a missing call cannot be dropped from the planned denominator or produce aggregate quality", async (t) => {
  const fixture = await reportFixture(t);
  fixture.calls.pop();
  await fixture.save();
  const report = await fixture.score();
  assertNoHeadline(report);
  assert.ok(
    report.integrity_errors.some((error) => /missing call/.test(error)),
  );
  assert.match(
    await readFile(join(fixture.directory, "report.md"), "utf8"),
    /N\/A — invalid experiment/,
  );
});

test("successful raw calls with missing or invalid latency invalidate the standalone ZG report", async (t) => {
  const fixture = await reportFixture(t);
  const inventory = { entries: [{ path: "entry.py", sha256: "a".repeat(64) }] };
  inventory.sha256 = objectHash(inventory.entries);
  Object.assign(fixture.manifest, {
    preparation_status: "ready",
    post_run_integrity: "verified",
    corpus_sha256: inventory.sha256,
    model_files_sha256: inventory.sha256,
    index_selection_audit: {
      content: "code",
      max_file_size_bytes: suite.protocol.index_selection.max_file_size_bytes,
      requested_extensions:
        suite.protocol.index_selection.code_extensions.length,
      verified: "request_arguments_and_public_status",
    },
  });
  delete fixture.manifest.preparation_error;
  for (const name of [
    "corpus.json",
    "corpus-after.json",
    "model-files.json",
    "model-files-after.json",
  ])
    await writeJson(join(fixture.shard, name), inventory);
  for (const phase of ["before", "after"]) {
    const directory = join(fixture.shard, "stages", phase);
    await mkdir(directory, { recursive: true });
    await writeFile(join(directory, "status.txt"), "Workspace index: ready\n");
    // Current snapshots hash stderr diagnostics; historical snapshots only
    // hashed stdout and normalized status. Both remain valid report inputs.
    const artifactNames = ["status.txt", "status.json"];
    if (phase === "before") {
      await writeFile(
        join(directory, "status.stderr.txt"),
        "Status diagnostics: indexed=true\n",
      );
      artifactNames.push("status.stderr.txt");
    }
    await writeJson(join(directory, "status.json"), {
      state: "ready",
      root: "/app",
      index_path: "/app/.zvec-grep",
      embedding: suite.protocol.model,
      files_scanned: 1,
      files_indexed: 1,
      files_pending: 0,
      files_failed: 0,
      entities_indexed: 1,
      indexed_source_bytes: 1,
    });
    const artifacts = Object.fromEntries(
      await Promise.all(
        artifactNames.map(async (name) => [
          name,
          await fileHash(join(directory, name)),
        ]),
      ),
    );
    await writeJson(join(directory, "summary.json"), {
      schema_version: 2,
      kind: "rust-public-status",
      logical_content_sha256: fixture.manifest.index_content_sha256,
      artifacts,
    });
  }
  for (const call of fixture.calls) {
    delete call.preparation_error;
    await writeJson(join(fixture.shard, call.raw_path), {
      content: [{ type: "text", text: "freshness: fresh\nNo matches." }],
    });
    call.raw_sha256 = await fileHash(join(fixture.shard, call.raw_path));
  }
  await fixture.save();
  assert.equal((await fixture.score()).integrity_passed, true);
  const diagnosticPath = join(fixture.shard, "stages/before/status.stderr.txt");
  const diagnostics = await readFile(diagnosticPath, "utf8");
  await writeFile(diagnosticPath, "tampered diagnostics\n");
  const tampered = await fixture.score();
  assertNoHeadline(tampered);
  assert.ok(
    tampered.integrity_errors.some((error) =>
      error.includes("snapshot artifact drift: status.stderr.txt"),
    ),
  );
  await writeFile(diagnosticPath, diagnostics);
  for (const invalid of [undefined, null, -1, "slow"]) {
    fixture.calls[1].latency_ms = invalid;
    await fixture.save();
    assertNoHeadline(await fixture.score());
    const scored = (
      await readFile(join(fixture.directory, "scores.jsonl"), "utf8")
    )
      .trim()
      .split("\n")
      .map(JSON.parse);
    assert.match(
      scored.find((row) => row.mode === "hybrid" && row.repetition === 2)
        .invalid_reason,
      /invalid call latency/,
    );
  }
  fixture.calls[1].latency_ms = 0;
  await fixture.save();
  assert.equal((await fixture.score()).integrity_passed, true);
});

test("a duplicate cannot replace a missing repetition even if the call count still equals the plan", async (t) => {
  const fixture = await reportFixture(t);
  fixture.calls[4] = clone(fixture.calls[0]);
  await fixture.save();
  const report = await fixture.score();
  assertNoHeadline(report);
  assert.ok(
    report.integrity_errors.some((error) => /duplicate call/.test(error)),
  );
  assert.ok(
    report.integrity_errors.some((error) => /missing call/.test(error)),
  );
});

test("missing fts/vector arms cannot silently become a valid hybrid-only run", async (t) => {
  const fixture = await reportFixture(t);
  fixture.calls.splice(5);
  await fixture.save();
  const report = await fixture.score();
  assertNoHeadline(report);
  assert.ok(
    report.integrity_errors.some((error) => /missing call/.test(error)),
  );
});

for (const [label, mutate] of [
  [
    "missing preview identity",
    (fixture) => {
      delete fixture.calls[1].preview;
    },
  ],
  [
    "unknown preview identity",
    (fixture) => {
      fixture.calls[1].preview = "none";
    },
  ],
  [
    "mismatched preview request",
    (fixture) => {
      fixture.calls[1].request.arguments.preview = "short";
    },
  ],
  [
    "reused raw response across modes",
    (fixture) => {
      fixture.calls[5].raw_path = fixture.calls[0].raw_path;
    },
  ],
  [
    "wrong manifest preview",
    (fixture) => {
      fixture.manifest.preview = "short";
    },
  ],
  [
    "missing manifest mode",
    (fixture) => {
      fixture.manifest.modes = ["hybrid"];
    },
  ],
  [
    "mixed search route",
    (fixture) => {
      fixture.calls[5].request.arguments.query = fixedTask.query;
    },
  ],
  [
    "symbol routing enabled",
    (fixture) => {
      fixture.calls[5].request.arguments.preferSymbol = true;
    },
  ],
  [
    "changed mode execution order",
    (fixture) => {
      [fixture.calls[4], fixture.calls[5]] = [
        fixture.calls[5],
        fixture.calls[4],
      ];
    },
  ],
  [
    "wrong corpus root",
    (fixture) => {
      fixture.calls[1].request.arguments.root = "/different-corpus";
    },
  ],
  [
    "unverified ready run",
    (fixture) => {
      fixture.manifest.preparation_status = "ready";
      delete fixture.manifest.preparation_error;
    },
  ],
  [
    "rewritten query",
    (fixture) => {
      fixture.calls[1].request.arguments.query += " source path hint";
    },
  ],
  [
    "extra subquery",
    (fixture) => {
      fixture.calls[1].request.arguments.queries = ["where is the symbol"];
    },
  ],
  [
    "wrong quality repetition",
    (fixture) => {
      fixture.calls[1].quality_observation = true;
    },
  ],
  [
    "stale suite identity",
    (fixture) => {
      fixture.manifest.suite = {
        ...fixture.manifest.suite,
        gold: "f".repeat(64),
      };
    },
  ],
  [
    "changed protocol",
    (fixture) => {
      fixture.manifest.protocol = { ...fixture.manifest.protocol, limit: 20 };
    },
  ],
  [
    "raw path traversal",
    (fixture) => {
      fixture.calls[1].raw_path = "../outside.json";
    },
  ],
]) {
  test(`offline aggregation withholds quality for ${label}`, async (t) => {
    const fixture = await reportFixture(t);
    mutate(fixture);
    await fixture.save();
    assertNoHeadline(await fixture.score());
  });
}

test("raw response tampering is detected before scoring even when replacement is another product error", async (t) => {
  const fixture = await reportFixture(t);
  await writeJson(join(fixture.shard, fixture.calls.at(-1).raw_path), {
    isError: true,
    content: [{ type: "text", text: "Changed after capture" }],
  });
  const report = await fixture.score();
  assertNoHeadline(report);
  assert.match(
    report.tasks.find((task) => task.mode === "vector").invalid_reason,
    /raw response changed since capture/,
  );
});
