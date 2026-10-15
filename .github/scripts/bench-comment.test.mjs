import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  authorize,
  finish,
  isBenchCommand,
  loadReports,
  prepare,
  resultComment,
} from "./bench-comment.mjs";

const sha = "a".repeat(40);
const newerSha = "b".repeat(40);
const baseSha = "c".repeat(40);

test("PR comment jobs have write permission and the run title retains its PR number", async () => {
  const workflow = await readFile(
    new URL("../workflows/zg-bench.yml", import.meta.url),
    "utf8",
  );
  assert.match(workflow, /^run-name: "ZG Bench · PR #\$\{\{/m);
  for (const name of ["request", "reply"]) {
    const job = workflow
      .split(`\n  ${name}:\n`)[1]
      .split(/\n {2}[a-z][\w-]*:\n/)[0];
    assert.match(job, /pull-requests: write/);
    assert.match(job, /issues: write/);
  }
  for (const name of ["retrieval", "e2e"]) {
    const job = workflow
      .split(`\n  ${name}:\n`)[1]
      .split(/\n {2}[a-z][\w-]*:\n/)[0];
    assert.doesNotMatch(job, /: write/);
  }
});
const request = {
  accepted: "true",
  candidate_sha: sha,
  candidate_repository: "contributor/zg",
  baseline_sha: baseSha,
  baseline_repository: "Cuiyus/zvec-grep",
  run_e2e: "false",
  pr_number: "7",
  comment_id: "42",
};
const retrieval = {
  json: {
    kind: "retrieval-version-comparison",
    baseline: { repository: request.baseline_repository, commit: baseSha },
    candidate: { repository: request.candidate_repository, commit: sha },
    status: "success",
  },
  markdown: "| Suite | Hit@5 |\n| SWE-QA20 | 0.8 |",
};
const e2e = {
  json: {
    schema_version: 3,
    cases: Array.from({ length: 20 }, (_, i) => ({ task_id: String(i) })),
    gate: { missing_tasks: [], passed: true },
  },
  identity: { candidate_commit: sha },
  markdown:
    "| Case | Judge | input_token | toolcall | time (s) |\n| Aggregate | 8 / 9 / +1 | 100 / 80 / -20% | 10 / 8 / -20% | 60 / 50 / -16.67% |",
};

function fixture() {
  const calls = [],
    comments = [],
    outputs = {},
    failures = [];
  const roles = { maintainer: "maintain", admin: "admin" };
  const context = {
    actor: "maintainer",
    eventName: "issue_comment",
    repo: { owner: "Cuiyus", repo: "zvec-grep" },
    serverUrl: "https://github.com",
    runId: 123,
    payload: {
      action: "created",
      issue: { number: 7, pull_request: {} },
      comment: {
        body: "@zg-bench",
        user: { type: "User", login: "maintainer" },
      },
    },
  };
  const pr = {
    state: "open",
    base: { sha: baseSha, repo: { full_name: "Cuiyus/zvec-grep" } },
    head: { sha, repo: { full_name: "contributor/zg" } },
  };
  const github = {
    rest: {
      repos: {
        async getCollaboratorPermissionLevel({ username }) {
          calls.push(["permission", username]);
          return { data: { role_name: roles[username] ?? "read" } };
        },
      },
      pulls: {
        async get(input) {
          calls.push(["pr", input]);
          return { data: pr };
        },
      },
      issues: {
        listComments() {},
        async createComment(input) {
          calls.push(["create", input]);
          comments.push({
            id: 42,
            body: input.body,
            user: { login: "github-actions[bot]" },
          });
          return { data: { id: 42 } };
        },
        async updateComment(input) {
          calls.push(["update", input]);
          return { data: { id: input.comment_id } };
        },
      },
    },
    async paginate(_method, input) {
      calls.push(["list", input]);
      return comments;
    },
  };
  const core = {
    setOutput(name, value) {
      outputs[name] = value;
    },
    setFailed(message) {
      failures.push(message);
    },
    summary: {
      addRaw() {
        return this;
      },
      async write() {},
    },
  };
  const env = { BENCH_TRIGGERING_ACTOR: "maintainer", PR_NUMBER: "7" };
  return {
    github,
    context,
    core,
    env,
    calls,
    outputs,
    roles,
    comments,
    pr,
    failures,
  };
}

test("only standalone, unquoted commands outside fenced code are accepted", () => {
  for (const body of [
    "@zg-bench",
    "  @zg-bench \r\n",
    "Please test:\n@zg-bench",
    "```\nexample\n```\n@zg-bench",
    "~~~js\nexample\n~~~\n@zg-bench",
  ])
    assert.equal(isBenchCommand(body), true, body);
  for (const body of [
    "please @zg-bench",
    "@zg-benchmark",
    "`@zg-bench`",
    "> @zg-bench",
    "    @zg-bench",
    "@zg-bench && echo bad",
    "```\n@zg-bench\n```",
    "~~~\n@zg-bench\n~~~",
    null,
  ])
    assert.equal(isBenchCommand(body), false, body);
});

test("ordinary comments, issue comments, edits, and bot comments cause no API writes or benchmark outputs", async () => {
  for (const change of [
    (x) => {
      x.context.payload.comment.body = "example @zg-bench";
    },
    (x) => {
      delete x.context.payload.issue.pull_request;
    },
    (x) => {
      x.context.payload.action = "edited";
    },
    (x) => {
      x.context.payload.comment.user.type = "Bot";
    },
    (x) => {
      x.context.payload.comment.user.login = "other";
    },
  ]) {
    const x = fixture();
    change(x);
    await prepare(x);
    assert.deepEqual(x.calls, []);
    assert.deepEqual(x.outputs, { accepted: "false" });
  }
});

test("current permissions for both the original actor and rerunner are required", async () => {
  for (const role of ["read", "write", "triage", "none", "custom-maintainer"]) {
    const x = fixture();
    x.roles.maintainer = role;
    await assert.rejects(prepare(x), /admin or maintain/);
    assert.ok(!x.calls.some(([name]) => name === "pr" || name === "create"));
  }
  const x = fixture();
  x.env.BENCH_TRIGGERING_ACTOR = "reader";
  await assert.rejects(prepare(x), /reader requires/);
  assert.deepEqual(
    x.calls.map((call) => call[1]),
    ["maintainer", "reader"],
  );
  const missing = fixture();
  delete missing.env.BENCH_TRIGGERING_ACTOR;
  await assert.rejects(authorize(missing), /Missing/);
  const unavailable = fixture();
  unavailable.github.rest.repos.getCollaboratorPermissionLevel = async () => {
    throw new Error("403 unavailable");
  };
  await assert.rejects(prepare(unavailable), /403 unavailable/);
});

test("a maintainer command freezes the fork's SHA and creates one status comment", async () => {
  const x = fixture();
  await prepare(x);
  assert.deepEqual(x.outputs, request);
  assert.equal(x.calls.filter(([name]) => name === "permission").length, 1);
  const body = x.calls.find(([name]) => name === "create")[1].body;
  assert.match(body, /<!-- zg-bench:run:123 -->/);
  assert.ok(body.includes(sha));
  assert.match(body, /Retrieval.*Queued/);
  assert.match(body, /e2e.*Skipped/);
  assert.match(body, new RegExp(`Cuiyus/zvec-grep/commit/${baseSha}`));
  assert.match(body, new RegExp(`contributor/zg/commit/${sha}`));
  assert.match(body, /actions\/runs\/123/);
  await prepare(x);
  assert.equal(x.calls.filter(([name]) => name === "create").length, 1);
  assert.equal(x.calls.filter(([name]) => name === "update").length, 1);
});

test("manual dispatch resolves a PR and rejects invalid targets before starting tests", async () => {
  const x = fixture();
  x.context.eventName = "workflow_dispatch";
  await prepare(x);
  assert.equal(x.outputs.candidate_sha, sha);
  for (const value of ["0", "-1", "7; echo bad", "1\n2", "9007199254740992"]) {
    const invalid = fixture();
    invalid.context.eventName = "workflow_dispatch";
    invalid.env.PR_NUMBER = value;
    await assert.rejects(prepare(invalid), /Invalid PR number/);
  }
  for (const change of [
    (y) => {
      y.pr.state = "closed";
    },
    (y) => {
      y.pr.head.repo = null;
    },
    (y) => {
      y.pr.head.sha = "main";
    },
    (y) => {
      y.pr.base.repo.full_name = "another/repo";
    },
  ]) {
    const invalid = fixture();
    change(invalid);
    await assert.rejects(prepare(invalid));
    assert.ok(!invalid.calls.some(([name]) => name === "create"));
  }
});

test("a user's copied bot marker is never edited", async () => {
  const x = fixture();
  x.comments.push({
    id: 5,
    body: "<!-- zg-bench:run:123 -->",
    user: { login: "maintainer" },
  });
  await prepare(x);
  assert.equal(x.calls.filter(([name]) => name === "update").length, 0);
  assert.equal(x.calls.filter(([name]) => name === "create").length, 1);
});

test("e2e is skipped by default and only explicit manual dispatch can enable it", async () => {
  const x = fixture();
  x.env.RUN_E2E = "true";
  await prepare(x);
  assert.equal(x.outputs.run_e2e, "false");
  x.context.eventName = "workflow_dispatch";
  await prepare(x);
  assert.equal(x.outputs.run_e2e, "true");
  assert.match(
    x.calls.filter(([name]) => name === "create" || name === "update").at(-1)[1].body,
    /10 trials per profile; trim one lowest and one highest score per task\/profile/,
  );
  const result = resultComment({
    context: x.context,
    request,
    results: { retrieval: "success", e2e: "skipped" },
    retrieval,
    e2e: null,
    currentBaseSha: newerSha,
  });
  assert.equal(result.complete, true);
  assert.match(result.body, /e2e · Skipped/);
  assert.match(result.body, /PR has newer commits/);
});

test("both benchmark tables are included and newer PR commits are called out", () => {
  const x = fixture();
  const result = resultComment({
    context: x.context,
    request: { ...request, run_e2e: "true" },
    results: { retrieval: "success", e2e: "success" },
    retrieval,
    e2e,
    currentSha: newerSha,
  });
  assert.equal(result.complete, true);
  assert.ok(result.body.includes(retrieval.markdown));
  assert.ok(result.body.includes(e2e.markdown));
  assert.match(result.body, /PR has newer commits/);
  assert.ok(result.body.includes(sha));
});

test("legacy and current report schemas are accepted, unknown schemas are not", () => {
  for (const version of [2, 3, 99]) {
    const result = resultComment({
      context: fixture().context,
      request: { ...request, run_e2e: "true" },
      results: { retrieval: "success", e2e: "success" },
      retrieval,
      e2e: { ...e2e, json: { ...e2e.json, schema_version: version } },
    });
    assert.equal(result.complete, version !== 99);
  }
});

test("missing, partial, cancelled, and wrong-commit reports never appear successful", () => {
  const x = fixture();
  const base = {
    context: x.context,
    request: { ...request, run_e2e: "true" },
    results: { retrieval: "success", e2e: "success" },
    retrieval,
    e2e,
  };
  for (const changes of [
    { retrieval: null },
    { e2e: null },
    {
      retrieval: {
        ...retrieval,
        json: { ...retrieval.json, status: "failed" },
      },
    },
    { retrieval: { ...retrieval, json: { candidate: { commit: newerSha } } } },
    {
      retrieval: {
        ...retrieval,
        json: {
          ...retrieval.json,
          baseline: { ...retrieval.json.baseline, commit: newerSha },
        },
      },
    },
    { e2e: { ...e2e, identity: { candidate_commit: newerSha } } },
    { e2e: { ...e2e, json: { cases: [], gate: { missing_tasks: ["task"] } } } },
    { results: { retrieval: "cancelled", e2e: "failure" } },
  ]) {
    const result = resultComment({ ...base, ...changes });
    assert.equal(result.complete, false);
    assert.match(result.body, /^## ZG Benchmark · ❌ Failed \/ incomplete/m);
  }
});

test("oversize reports are bounded and raw mentions are neutralized", () => {
  const x = fixture();
  const result = resultComment({
    context: x.context,
    request: { ...request, run_e2e: "true" },
    results: { retrieval: "success", e2e: "success" },
    retrieval: { ...retrieval, markdown: "@someone\n" + "x".repeat(70000) },
    e2e: { ...e2e, markdown: "y".repeat(70000) },
  });
  assert.ok(result.body.length < 60000);
  assert.match(result.body, /Report truncated/);
  assert.ok(!result.body.includes("@someone"));
});

test("a single e2e artifact extracted directly into its target directory is loaded", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "bench-comment-flat-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const dir = join(root, "e2e");
  await mkdir(dir);
  await writeFile(join(dir, "report.json"), JSON.stringify(e2e.json));
  await writeFile(join(dir, "report.md"), e2e.markdown);
  const manifest = join(dir, "candidate-manifest.json");
  await writeFile(manifest, JSON.stringify(e2e.identity));
  assert.deepEqual((await loadReports(root, 123, sha)).e2e, e2e);
  await writeFile(manifest, JSON.stringify({ candidate_commit: newerSha }));
  assert.equal((await loadReports(root, 123, sha)).e2e, null);
});

test("a completed rerun aggregate replaces the earlier partial e2e comment", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "bench-comment-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  for (const [attempt, json] of [
    [
      1,
      {
        ...e2e.json,
        cases: e2e.json.cases.slice(0, 19),
        gate: { passed: false, missing_tasks: ["19"] },
      },
    ],
    [2, e2e.json],
  ]) {
    const directory = join(root, `e2e/swe-qa-aggregate-report-123-${attempt}`);
    await mkdir(directory, { recursive: true });
    await writeFile(join(directory, "report.json"), JSON.stringify(json));
    await writeFile(join(directory, "report.md"), e2e.markdown);
    await writeFile(
      join(directory, "candidate-manifest.json"),
      JSON.stringify(e2e.identity),
    );
  }
  const reports = await loadReports(root, 123, sha);
  assert.equal(reports.e2e.artifact_attempt, 2);
  const result = resultComment({
    context: fixture().context,
    request: { ...request, run_e2e: "true" },
    results: { retrieval: "success", e2e: "success" },
    ...reports,
    retrieval,
  });
  assert.equal(result.complete, true);
  assert.match(result.body, /e2e · ✅ Complete/);
});

test("final reply uses a matching earlier-attempt e2e artifact and updates the status comment", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "bench-comment-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  async function writeReport(dir, report, jsonName, mdName) {
    await mkdir(dir, { recursive: true });
    await writeFile(join(dir, jsonName), JSON.stringify(report.json));
    await writeFile(join(dir, mdName), report.markdown);
    if (report.identity)
      await writeFile(
        join(dir, "candidate-manifest.json"),
        JSON.stringify(report.identity),
      );
  }
  await writeReport(
    join(root, "retrieval"),
    retrieval,
    "summary.json",
    "summary.md",
  );
  await writeReport(
    join(root, "e2e/swe-qa-aggregate-report-123-1"),
    e2e,
    "report.json",
    "report.md",
  );
  await writeReport(
    join(root, "e2e/swe-qa-aggregate-report-123-2"),
    { ...e2e, identity: { candidate_commit: newerSha } },
    "report.json",
    "report.md",
  );
  assert.deepEqual((await loadReports(root, 123, sha)).e2e, {
    ...e2e,
    artifact_attempt: 1,
  });
  const x = fixture();
  await prepare(x);
  x.env.BENCH_REPORTS = root;
  x.env.BENCH_NEEDS = JSON.stringify({
    request: { outputs: { ...request, run_e2e: "true" } },
    retrieval: { result: "success" },
    e2e: { result: "success" },
  });
  await finish(x);
  assert.equal(x.failures.length, 0);
  assert.equal(x.calls.filter(([name]) => name === "create").length, 1);
  assert.match(
    x.calls.find(([name]) => name === "update")[1].body,
    /ZG Benchmark · ✅ Complete/,
  );
  x.env.BENCH_REPORTS = join(root, "missing");
  await finish(x);
  assert.equal(x.failures.length, 1);
  assert.match(
    x.calls.filter(([name]) => name === "update").at(-1)[1].body,
    /Missing results are not zero scores/,
  );
});
