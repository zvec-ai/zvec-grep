import assert from "node:assert/strict";
import { mkdir, readFile, stat, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { authorize } from "./bench-comment.mjs";
import { readIndexReceipts } from "./browsecomp-index-store.mjs";
import {
  markdownReport,
  validateComparison,
} from "../../benchmarks/browse-comp-plus-retrieval/report.mjs";

export function isDocCommand(body) {
  let fence = null;
  return String(body ?? "")
    .split(/\r?\n/)
    .some((line) => {
      const match = /^ {0,3}(`{3,}|~{3,})/.exec(line);
      if (match) {
        const delimiter = match[1][0];
        if (!fence) fence = { delimiter, length: match[1].length };
        else if (
          delimiter === fence.delimiter &&
          match[1].length >= fence.length &&
          /^\s*$/.test(line.slice(match[0].length))
        )
          fence = null;
        return false;
      }
      return !fence && /^ {0,3}@zg-retrieval-bench[ \t]+doc[ \t]*$/.test(line);
    });
}

const marker = (context) => `<!-- zg-retrieval-bench:doc:${context.runId} -->`;
const url = (context) =>
  `${context.serverUrl}/${context.repo.owner}/${context.repo.repo}/actions/runs/${context.runId}`;

async function upsert({ github, context, request, body }) {
  const comments = await github.paginate(github.rest.issues.listComments, {
    ...context.repo,
    issue_number: Number(request.pr_number),
    per_page: 100,
  });
  const previous = comments.find(
    (comment) =>
      comment.user?.login === "github-actions[bot]" &&
      comment.body?.startsWith(marker(context)) &&
      (!request.comment_id ||
        String(comment.id) === String(request.comment_id)),
  );
  assert.ok(
    !request.comment_id || previous,
    "the recorded result comment is not our own bot comment",
  );
  const { data } = previous
    ? await github.rest.issues.updateComment({
        ...context.repo,
        comment_id: previous.id,
        body,
      })
    : await github.rest.issues.createComment({
        ...context.repo,
        issue_number: Number(request.pr_number),
        body,
      });
  return data.id;
}

export async function prepareDoc({ github, context, core, env }) {
  core.setOutput("accepted", "false");
  if (context.eventName === "issue_comment") {
    const { action, issue, comment } = context.payload;
    if (
      action !== "created" ||
      !issue?.pull_request ||
      comment?.user?.type !== "User" ||
      comment.user.login !== context.actor ||
      !isDocCommand(comment.body)
    )
      return null;
  } else assert.equal(context.eventName, "workflow_dispatch");
  await authorize({ github, context, env });
  const raw =
    context.eventName === "issue_comment"
      ? String(context.payload.issue.number)
      : env.PR_NUMBER;
  assert.match(raw ?? "", /^[1-9]\d*$/);
  const { data: pr } = await github.rest.pulls.get({
    ...context.repo,
    pull_number: Number(raw),
  });
  assert.equal(pr.state, "open");
  assert.equal(
    pr.base.repo.full_name.toLowerCase(),
    `${context.repo.owner}/${context.repo.repo}`.toLowerCase(),
  );
  for (const side of [pr.base, pr.head]) {
    assert.match(side.sha, /^[a-f0-9]{40}$/);
    assert.match(side.repo?.full_name ?? "", /^[\w.-]+\/[\w.-]+$/);
  }
  assert.match(env.BCP_HARNESS_SHA ?? "", /^[a-f0-9]{40}$/);
  const request = {
    schema_version: 1,
    repository: `${context.repo.owner}/${context.repo.repo}`,
    run_id: Number(context.runId),
    run_attempt: Number(env.GITHUB_RUN_ATTEMPT ?? 1),
    pr_number: Number(raw),
    baseline_sha: pr.base.sha,
    baseline_repository: pr.base.repo.full_name,
    candidate_sha: pr.head.sha,
    candidate_repository: pr.head.repo.full_name,
    harness_commit: env.BCP_HARNESS_SHA,
  };
  request.comment_id = await upsert({
    github,
    context,
    request,
    body: [
      marker(context),
      "## BrowseComp+ Retrieval-only · queued",
      "",
      `PR base Rust \`${pr.base.sha}\` → PR head Rust \`${pr.head.sha}\`.`,
      "",
      "A compatible full 100,195-document index Artifact is reused. Only a new underlying storage identity requires indexing. This independent task does not run the standard Retrieval-only suites.",
      "",
      `[Run and Artifacts](${url(context)})`,
    ].join("\n"),
  });
  await mkdir(env.BCP_REQUEST_DIRECTORY, { recursive: true });
  await writeFile(
    join(env.BCP_REQUEST_DIRECTORY, "request.json"),
    JSON.stringify(request, null, 2) + "\n",
  );
  for (const [key, value] of Object.entries({ ...request, accepted: "true" }))
    core.setOutput(key, String(value));
  return request;
}

export async function finishDoc({ github, context, env }) {
  await authorize({ github, context, env });
  const request = JSON.parse(env.BCP_REQUEST);
  assert.equal(request.accepted, "true");
  assert.match(request.pr_number, /^[1-9]\d*$/);
  assert.equal(Number(request.run_id), Number(context.runId));
  assert.equal(
    request.repository.toLowerCase(),
    `${context.repo.owner}/${context.repo.repo}`.toLowerCase(),
  );
  const jobs = JSON.parse(env.BCP_JOB_RESULTS);
  const catalog = [];
  let invalid;
  try {
    if (env.BCP_INDEX_DIRECTORY) {
      const plan = JSON.parse(env.BCP_INDEX_PLAN);
      const allowed = new Set(plan.include.map((row) => row.key));
      const receipts = await readIndexReceipts(env.BCP_INDEX_DIRECTORY, {
        missingOk: true,
      });
      for (const receipt of receipts.values()) {
        assert.ok(allowed.has(receipt.key));
        catalog.push(
          `<!-- zg-retrieval-bench:doc-index:${receipt.key}:${receipt.run_id}:${receipt.artifact_id} -->`,
        );
      }
    }
  } catch (error) {
    invalid = error;
  }
  let body =
    "# BrowseComp+ Retrieval-only · failed / incomplete\n\nThe run did not produce a complete validated comparison. Partial evidence is retained in Artifacts; missing data are not zero.\n";
  if (
    !invalid &&
    ["package", "plan", "index", "query"].every(
      (name) => jobs[name]?.result === "success",
    )
  ) {
    try {
      const path = join(env.BCP_REPORT_DIRECTORY, "summary.json");
      assert.ok(
        (await stat(path)).size < 5 * 1024 * 1024,
        "oversized benchmark report",
      );
      const report = JSON.parse(await readFile(path, "utf8"));
      validateComparison(report, request);
      body = markdownReport(report);
    } catch (error) {
      invalid = error;
    }
  }
  const { data: pr } = await github.rest.pulls.get({
    ...context.repo,
    pull_number: Number(request.pr_number),
  });
  const stale = pr.head.sha !== request.candidate_sha;
  const result = await upsert({
    github,
    context,
    request,
    body: [
      marker(context),
      ...(stale
        ? ["**Historical snapshot: the PR has newer commits.**", ""]
        : []),
      body,
      "",
      `[Run, Summary and Artifacts](${url(context)})`,
      ...catalog,
    ].join("\n"),
  });
  if (invalid)
    throw new Error(
      "BrowseComp report validation failed; the result comment was marked incomplete",
      { cause: invalid },
    );
  return result;
}
