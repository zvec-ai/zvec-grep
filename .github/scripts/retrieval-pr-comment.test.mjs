import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { publish, validateComparison } from "./retrieval-pr-comment.mjs";

const base = "a".repeat(40),
  head = "b".repeat(40);
const request = {
  schema_version: 1,
  run_id: 123,
  run_attempt: 1,
  pr_number: 7,
  baseline: { repository: "Cuiyus/zvec-grep", commit: base, ref: "main" },
  candidate: {
    repository: "contributor/zvec-grep",
    commit: head,
    ref: "feature",
  },
};
const models = {
  sweqa: "local/potion-code-16m-v2",
  beir: "local/potion-multilingual-128m",
  duretrieval: "local/potion-multilingual-128m",
  quarry: "local/potion-code-16m-v2",
};
const report = {
  schema_version: 1,
  kind: "retrieval-version-comparison",
  status: "success",
  baseline: { repository: request.baseline.repository, commit: base },
  candidate: { repository: request.candidate.repository, commit: head },
  harness_commit: "c".repeat(40),
  errors: [],
  rows: Object.entries(models).flatMap(([suite, model]) =>
    ["hybrid", "fts", "vector"].map((mode) => ({
      suite,
      mode,
      model,
      metrics: Object.fromEntries(
        [
          "file_hit_at_1",
          "file_hit_at_5",
          "file_hit_at_10",
          "file_mrr_at_10",
          "ndcg_at_10",
        ].map((metric) => [
          metric,
          { baseline: 0.5, candidate: 0.6, delta: 0.1 },
        ]),
      ),
    })),
  ),
};

async function fixture(t) {
  const directory = await mkdtemp(join(tmpdir(), "retrieval-pr-comment-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const run = {
    id: 123,
    run_attempt: 1,
    workflow_id: 99,
    path: ".github/workflows/retrieval-pr.yml",
    repository: { full_name: "Cuiyus/zvec-grep" },
    head_repository: { full_name: "contributor/zvec-grep" },
    event: "pull_request",
    status: "completed",
    conclusion: "success",
    head_sha: head,
    head_branch: "feature",
    pull_requests: [],
  };
  const pr = {
    state: "open",
    base: {
      sha: base,
      ref: "main",
      repo: { full_name: request.baseline.repository },
    },
    head: {
      sha: head,
      ref: "feature",
      repo: { full_name: request.candidate.repository },
    },
  };
  const context = {
    eventName: "workflow_run",
    repo: { owner: "Cuiyus", repo: "zvec-grep" },
    serverUrl: "https://github.com",
    payload: { workflow_run: { id: 123, run_attempt: 1 } },
  };
  const calls = [],
    comments = [],
    info = [];
  let prReads = 0;
  const x = { context, directory, run, pr, calls, comments, info };
  x.github = {
    rest: {
      actions: {
        async getWorkflowRun(input) {
          assert.equal(input.run_id, 123);
          return { data: run };
        },
        async getWorkflow(input) {
          assert.equal(input.workflow_id, "retrieval-pr.yml");
          return { data: { id: 99 } };
        },
      },
      pulls: {
        async get(input) {
          calls.push(["pr", input]);
          prReads++;
          if (prReads === 2) x.beforeWrite?.();
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
        },
        async updateComment(input) {
          calls.push(["update", input]);
          comments.find((c) => c.id === input.comment_id).body = input.body;
        },
      },
    },
    async paginate(_method, input) {
      calls.push(["list", input]);
      return comments;
    },
  };
  x.core = {
    info(message) {
      info.push(message);
    },
    summary: {
      addRaw() {
        return this;
      },
      async write() {},
    },
  };
  x.write = async (file, value) => {
    const full = join(directory, file);
    await mkdir(join(full, ".."), { recursive: true });
    await writeFile(full, JSON.stringify(value));
  };
  await x.write("context/request.json", request);
  await x.write("comparison/summary.json", report);
  x.messages = () =>
    calls.filter(([name]) => ["create", "update"].includes(name));
  return x;
}

test("a fork PR gets all twelve comparison rows, commit links and a source run link", async (t) => {
  const x = await fixture(t);
  await publish(x);
  assert.equal(x.messages().length, 1);
  const [action, input] = x.messages()[0];
  assert.equal(action, "create");
  assert.equal(input.issue_number, 7);
  assert.match(input.body, /✅ Complete/);
  assert.match(input.body, /0\.5000 \/ 0\.6000 \/ \+0\.1000/);
  assert.equal(
    input.body
      .split("\n")
      .filter((line) => /^\| (SWE-QA20|BEIR|DuRetrieval|Quarry)/.test(line))
      .length,
    12,
  );
  assert.ok(input.body.includes(`/commit/${base}`));
  assert.ok(input.body.includes(`/commit/${head}`));
  assert.match(input.body, /actions\/runs\/123\/attempts\/1/);
  assert.match(input.body, /Embedding: local\. e2e: skipped/);
});

test("later results and failed-job reruns update one bot comment", async (t) => {
  const x = await fixture(t);
  await publish(x);
  x.run.run_attempt = x.context.payload.workflow_run.run_attempt = 2;
  // The successful context job is retained from attempt 1 on failed-job reruns.
  await publish(x);
  assert.deepEqual(
    x.messages().map(([action]) => action),
    ["create", "update"],
  );
  assert.match(x.messages()[1][1].body, /zg-retrieval-run:123:2/);
});

test("user-copied and @zg-bench comment markers are never edited", async (t) => {
  const x = await fixture(t);
  x.comments.push(
    { id: 1, user: { login: "contributor" }, body: "<!-- zg-retrieval-pr -->" },
    {
      id: 2,
      user: { login: "github-actions[bot]" },
      body: "<!-- zg-bench:run:123 -->",
    },
  );
  await publish(x);
  assert.equal(x.messages()[0][0], "create");
});

test("missing, malformed or invalid comparison artifacts publish incomplete without scores", async (t) => {
  for (const value of [null, {}, { ...report, status: "failed" }]) {
    const x = await fixture(t);
    await x.write("comparison/summary.json", value);
    await publish(x);
    assert.match(x.messages()[0][1].body, /Failed \/ incomplete/);
    assert.match(x.messages()[0][1].body, /not zero scores/);
    assert.doesNotMatch(x.messages()[0][1].body, /0\.5000|\| Suite \|/);
  }
  const x = await fixture(t);
  await rm(join(x.directory, "comparison"), { recursive: true });
  await publish(x);
  assert.match(x.messages()[0][1].body, /Failed \/ incomplete/);
});

test("oversized report data is withheld instead of being loaded into a PR comment", async (t) => {
  const x = await fixture(t);
  await writeFile(
    join(x.directory, "comparison/summary.json"),
    " ".repeat(128 * 1024 + 1),
  );
  await publish(x);
  assert.match(x.messages()[0][1].body, /Failed \/ incomplete/);
});

test("a failed source run cannot promote an earlier successful artifact", async (t) => {
  const x = await fixture(t);
  x.run.conclusion = "failure";
  await publish(x);
  assert.match(x.messages()[0][1].body, /conclusion: \*\*failure\*\*/);
  assert.doesNotMatch(x.messages()[0][1].body, /✅ Complete|\| Suite \|/);
});

test("cancelled runs, unfinished runs and superseded attempts do not comment", async (t) => {
  for (const change of [
    { conclusion: "cancelled" },
    { status: "in_progress" },
    { run_attempt: 2 },
  ]) {
    const x = await fixture(t);
    Object.assign(x.run, change);
    await publish(x);
    assert.equal(x.messages().length, 0);
  }
});

test("closed PRs, newer base/head, a different branch or repository are skipped", async (t) => {
  for (const change of [
    (pr) => {
      pr.state = "closed";
    },
    (pr) => {
      pr.head.sha = "d".repeat(40);
    },
    (pr) => {
      pr.base.sha = "d".repeat(40);
    },
    (pr) => {
      pr.head.ref = "another-branch";
    },
    (pr) => {
      pr.head.repo.full_name = "another/repository";
    },
    (pr) => {
      pr.base.repo.full_name = "another/repository";
    },
  ]) {
    const x = await fixture(t);
    change(x.pr);
    await publish(x);
    assert.equal(x.messages().length, 0);
  }
});

test("a push during publication prevents overwriting the current result", async (t) => {
  const x = await fixture(t);
  x.beforeWrite = () => {
    x.pr.head.sha = "d".repeat(40);
  };
  await publish(x);
  assert.equal(x.messages().length, 0);
});

test("older run IDs or attempts cannot overwrite a newer bot comment", async (t) => {
  for (const version of ["124:1", "123:2"]) {
    const x = await fixture(t);
    x.comments.push({
      id: 42,
      user: { login: "github-actions[bot]" },
      body: `<!-- zg-retrieval-pr -->\n<!-- zg-retrieval-run:${version} -->`,
    });
    await publish(x);
    assert.equal(x.messages().length, 0);
  }
});

test("wrong workflow/event/repository and invalid PR context never get write operations", async (t) => {
  for (const change of [
    (x) => {
      x.context.eventName = "pull_request";
    },
    (x) => {
      x.run.event = "workflow_dispatch";
    },
    (x) => {
      x.run.workflow_id = 100;
    },
    (x) => {
      x.run.path = ".github/workflows/spoof.yml";
    },
    (x) => {
      x.run.repository.full_name = "another/repo";
    },
  ]) {
    const x = await fixture(t);
    change(x);
    await assert.rejects(publish(x));
    assert.equal(x.messages().length, 0);
  }
  for (const change of [
    { run_id: 124 },
    { run_attempt: 2 },
    { pr_number: "7" },
    { candidate: { ...request.candidate, commit: "d".repeat(40) } },
    { candidate: { ...request.candidate, repository: "another/repo" } },
    { candidate: { ...request.candidate, ref: "another" } },
    { baseline: { ...request.baseline, repository: "another/repo" } },
  ]) {
    const x = await fixture(t);
    await x.write("context/request.json", { ...request, ...change });
    await assert.rejects(publish(x));
    assert.equal(x.messages().length, 0);
  }
});

test("source run PR associations must agree with the context artifact", async (t) => {
  const x = await fixture(t);
  x.run.pull_requests = [{ number: 8, head: x.pr.head, base: x.pr.base }];
  await assert.rejects(publish(x), /disagrees/);
  x.run.pull_requests[0].number = 7;
  await publish(x);
  assert.equal(x.messages().length, 1);
});

test("missing context cannot choose a target PR", async (t) => {
  const x = await fixture(t);
  await rm(join(x.directory, "context"), { recursive: true });
  await assert.rejects(publish(x), /Missing PR context/);
  assert.equal(x.messages().length, 0);
});

test("the renderer rejects wrong identities, coverage, models, nonfinite metrics and false deltas", () => {
  for (const mutate of [
    (r) => {
      r.candidate.commit = "d".repeat(40);
    },
    (r) => {
      r.baseline.repository = "another/repo";
    },
    (r) => {
      r.rows.pop();
    },
    (r) => {
      r.rows[0] = structuredClone(r.rows[1]);
    },
    (r) => {
      r.rows[0].model = "remote/model";
    },
    (r) => {
      r.rows[0].metrics.ndcg_at_10.baseline = NaN;
    },
    (r) => {
      r.rows[0].metrics.ndcg_at_10.candidate = 1.1;
    },
    (r) => {
      r.rows[0].metrics.ndcg_at_10.delta = 0;
    },
    (r) => {
      r.errors = ["@someone malicious content"];
    },
  ]) {
    const invalid = structuredClone(report);
    mutate(invalid);
    assert.throws(() => validateComparison(invalid, request));
  }
});

test("artifact Markdown, URLs and arbitrary fields never appear in a result comment", async (t) => {
  const x = await fixture(t);
  await x.write("comparison/summary.json", {
    ...report,
    run_url: "https://evil.invalid",
    server_url: "https://evil.invalid",
    markdown: "@everyone forged result",
  });
  await writeFile(
    join(x.directory, "comparison/summary.md"),
    "@everyone forged result",
  );
  await publish(x);
  assert.doesNotMatch(x.messages()[0][1].body, /evil|everyone|forged/);
});

test("publisher checks out trusted code and only reads artifacts from the triggering run", async () => {
  const workflow = await readFile(
    new URL("../workflows/retrieval-pr-comment.yml", import.meta.url),
    "utf8",
  );
  assert.match(workflow, /workflows: \[Retrieval-only PR\]/);
  assert.match(workflow, /types: \[completed\]/);
  assert.match(
    workflow,
    /if: github\.event\.workflow_run\.event == 'pull_request'/,
  );
  assert.match(workflow, /actions: read/);
  assert.match(workflow, /issues: write/);
  assert.match(workflow, /pull-requests: write/);
  assert.equal(
    (
      workflow.match(/run-id: \$\{\{ github\.event\.workflow_run\.id \}\}/g) ??
      []
    ).length,
    2,
  );
  assert.doesNotMatch(
    workflow,
    /ref:|head_sha|head_repository.*\/|secrets\.|npm |cache@|pull_request_target/,
  );
  assert.match(workflow, /cancel-in-progress: false/);
});

test("the read-only context job records the original fork PR event's exact versions", async (t) => {
  const x = await fixture(t);
  const workflow = await readFile(
    new URL("../workflows/retrieval-pr.yml", import.meta.url),
    "utf8",
  );
  const job = workflow
    .split("  comment-context:\n")[1]
    .split("\n  retrieval:\n")[0];
  assert.match(job, /if: github\.event_name == 'pull_request'/);
  assert.match(job, /name: retrieval-pr-context/);
  assert.doesNotMatch(job, /: write|secrets\.|checkout/);
  const script = /<<'JS'\n([\s\S]*?)\n {10}JS/
    .exec(job)[1]
    .replace(/^ {10}/gm, "");
  const event = join(x.directory, "event.json");
  await writeFile(
    event,
    JSON.stringify({ pull_request: { number: 7, ...x.pr } }),
  );
  const result = spawnSync(process.execPath, ["--input-type=module"], {
    input: script,
    encoding: "utf8",
    env: {
      ...process.env,
      GITHUB_EVENT_PATH: event,
      RUNNER_TEMP: x.directory,
      GITHUB_RUN_ID: "123",
      GITHUB_RUN_ATTEMPT: "1",
    },
  });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(
    JSON.parse(
      await readFile(
        join(x.directory, "retrieval-pr-context/request.json"),
        "utf8",
      ),
    ),
    request,
  );
});
