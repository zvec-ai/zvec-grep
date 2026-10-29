import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  prepareDoc,
  finishDoc,
  isDocCommand,
} from "./browsecomp-doc-comment.mjs";
import {
  comparisonFixture,
  requestFixture,
} from "../../benchmarks/browse-comp-plus-retrieval/test/comparison-fixture.mjs";
import {
  markdownReport,
  validateComparison,
} from "../../benchmarks/browse-comp-plus-retrieval/report.mjs";

async function fixture(t) {
  const directory = await mkdtemp(join(tmpdir(), "browsecomp-comment-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const outputs = {},
    comments = [],
    roles = { maintainer: "maintain" };
  const context = {
    actor: "maintainer",
    eventName: "issue_comment",
    repo: { owner: "owner", repo: "repo" },
    runId: 123,
    serverUrl: "https://github.com",
    payload: {
      action: "created",
      issue: { number: 7, pull_request: {} },
      comment: {
        body: "@zg-retrieval-bench doc",
        user: { type: "User", login: "maintainer" },
      },
    },
  };
  const pr = {
    state: "open",
    base: {
      sha: requestFixture.baseline_sha,
      repo: { full_name: "owner/repo" },
    },
    head: {
      sha: requestFixture.candidate_sha,
      repo: { full_name: "contributor/repo" },
    },
  };
  const github = {
    paginate: async () => comments,
    rest: {
      repos: {
        getCollaboratorPermissionLevel: async ({ username }) => ({
          data: { role_name: roles[username] ?? "read" },
        }),
      },
      pulls: { get: async () => ({ data: pr }) },
      issues: {
        listComments() {},
        createComment: async ({ body }) => {
          const row = { id: 42, body, user: { login: "github-actions[bot]" } };
          comments.push(row);
          return { data: row };
        },
        updateComment: async ({ body, comment_id }) => {
          const row = comments.find((c) => c.id === comment_id);
          row.body = body;
          return { data: row };
        },
      },
    },
  };
  const env = {
    BENCH_TRIGGERING_ACTOR: "maintainer",
    BCP_HARNESS_SHA: requestFixture.harness_commit,
    BCP_REQUEST_DIRECTORY: directory,
    BCP_REPORT_DIRECTORY: directory,
    BCP_JOB_RESULTS: JSON.stringify(
      Object.fromEntries(
        ["package", "plan", "index", "query"].map((name) => [
          name,
          { result: "success" },
        ]),
      ),
    ),
  };
  return {
    github,
    context,
    env,
    core: { setOutput: (key, value) => (outputs[key] = value) },
    outputs,
    comments,
    roles,
    pr,
    directory,
  };
}

test("only a standalone unfenced doc command triggers the independent task", () => {
  for (const text of [
    "@zg-retrieval-bench doc",
    "  @zg-retrieval-bench\tdoc\n",
  ])
    assert.ok(isDocCommand(text));
  for (const text of [
    "@zg-bench",
    "@zg-retrieval-bench",
    "@zg-retrieval-bench doc extra",
    "> @zg-retrieval-bench doc",
    "    @zg-retrieval-bench doc",
    "`@zg-retrieval-bench doc`",
    "```sh\n@zg-retrieval-bench doc\n```",
    "please @zg-retrieval-bench doc",
  ])
    assert.equal(isDocCommand(text), false, text);
});

test("the request pins both Rust sources and records one bot comment", async (t) => {
  const x = await fixture(t);
  const request = await prepareDoc(x);
  assert.equal(x.outputs.accepted, "true");
  assert.equal(request.baseline_sha, x.pr.base.sha);
  assert.equal(request.candidate_sha, x.pr.head.sha);
  assert.equal(x.comments.length, 1);
  assert.equal(
    JSON.parse(await readFile(join(x.directory, "request.json")))
      .harness_commit,
    requestFixture.harness_commit,
  );
});

test("unrelated commands and bots do no work; insufficient original/rerun roles are denied", async (t) => {
  for (const change of [
    (x) => (x.context.payload.comment.body = "@zg-bench"),
    (x) => (x.context.payload.comment.user.type = "Bot"),
    (x) => (x.context.payload.issue.pull_request = null),
  ]) {
    const x = await fixture(t);
    change(x);
    assert.equal(await prepareDoc(x), null);
    assert.equal(x.comments.length, 0);
    assert.equal(x.outputs.accepted, "false");
  }
  const x = await fixture(t);
  x.env.BENCH_TRIGGERING_ACTOR = "reader";
  await assert.rejects(prepareDoc(x), /admin or maintain/);
  assert.equal(x.comments.length, 0);
});

test("typed results update the running comment and expose historical index provenance", async (t) => {
  const x = await fixture(t);
  await prepareDoc(x);
  x.env.BCP_REQUEST = JSON.stringify(x.outputs);
  await writeFile(
    join(x.directory, "summary.json"),
    JSON.stringify(comparisonFixture()),
  );
  await finishDoc(x);
  assert.equal(x.comments.length, 1);
  assert.match(x.comments[0].body, /historical Artifact-builder/);
  assert.match(x.comments[0].body, /-50\.0%/);
  assert.match(x.comments[0].body, /same sealed full-corpus index/);
  assert.match(x.comments[0].body, /## Query/);
  assert.match(x.comments[0].body, /## Retrieval quality · Evidence/);
  assert.doesNotMatch(x.comments[0].body, /Recall@|Gold/);
});

test("older Gold summaries cannot publish an Evidence comparison", async (t) => {
  const x = await fixture(t);
  await prepareDoc(x);
  x.env.BCP_REQUEST = JSON.stringify(x.outputs);
  const report = comparisonFixture();
  delete report.quality_target;
  await writeFile(join(x.directory, "summary.json"), JSON.stringify(report));
  await assert.rejects(finishDoc(x), /validation failed/);
  assert.doesNotMatch(x.comments[0].body, /✅|## Retrieval quality · Evidence/);
});

test("missing/failed jobs never publish green and malformed success data replaces queued with failure", async (t) => {
  const x = await fixture(t);
  await prepareDoc(x);
  x.env.BCP_REQUEST = JSON.stringify(x.outputs);
  x.env.BCP_JOB_RESULTS = JSON.stringify({ query: { result: "cancelled" } });
  await finishDoc(x);
  assert.doesNotMatch(x.comments[0].body, /✅/);
  x.env.BCP_JOB_RESULTS = JSON.stringify(
    Object.fromEntries(
      ["package", "plan", "index", "query"].map((n) => [
        n,
        { result: "success" },
      ]),
    ),
  );
  const bad = comparisonFixture();
  bad.candidate.commit = "e".repeat(40);
  await writeFile(join(x.directory, "summary.json"), JSON.stringify(bad));
  await assert.rejects(finishDoc(x), /validation failed/);
  assert.match(x.comments[0].body, /failed \/ incomplete/);
});

test("percentiles and identities are validated before rendering untrusted report JSON", () => {
  const report = comparisonFixture();
  validateComparison(report, requestFixture);
  assert.match(markdownReport(report), /Historical Index time, RSS, size/);
  for (const mutate of [
    (r) => delete r.quality_target,
    (r) => (r.quality_target = "gold"),
    (r) => delete r.baseline.quality_target,
    (r) => (r.candidate.quality_target = "gold"),
    (r) => (r.model_sha256 = "f".repeat(64)),
    (r) => (r.candidate.performance.query.fts.latency_ms.count = 49),
    (r) => (r.baseline.performance.query.hybrid.latency_ms.p95 = "<script>"),
    (r) => (r.run_url = "javascript:evil"),
    (r) => (r.candidate.index_artifact.artifact_id = -1),
  ]) {
    const bad = structuredClone(report);
    mutate(bad);
    assert.throws(() => validateComparison(bad, requestFixture));
  }
});

test("successful Index receipts survive a failed Query in the durable result comment", async (t) => {
  for (const flat of [true, false]) {
    await t.test(
      flat ? "v8 flattened single receipt" : "named receipt directory",
      async (t) => {
        const x = await fixture(t);
        await prepareDoc(x);
        x.env.BCP_REQUEST = JSON.stringify(x.outputs);
        x.env.BCP_JOB_RESULTS = JSON.stringify({
          query: { result: "failure" },
        });
        const key = "d".repeat(64);
        x.env.BCP_INDEX_PLAN = JSON.stringify({ include: [{ key }] });
        x.env.BCP_INDEX_DIRECTORY = join(x.directory, "receipts");
        const directory = flat
          ? x.env.BCP_INDEX_DIRECTORY
          : join(x.env.BCP_INDEX_DIRECTORY, `browsecomp-doc-index-ref-${key}`);
        await mkdir(directory, { recursive: true });
        await writeFile(
          join(directory, "receipt.json"),
          JSON.stringify({
            key,
            name: `browsecomp-doc-index-${key}`,
            artifact_id: 10,
            run_id: 100,
          }),
        );
        await finishDoc(x);
        assert.match(x.comments[0].body, new RegExp(`doc-index:${key}:100:10`));
        assert.doesNotMatch(x.comments[0].body, /✅/);
      },
    );
  }
});
