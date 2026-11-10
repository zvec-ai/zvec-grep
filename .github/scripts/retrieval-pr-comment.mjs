import assert from "node:assert/strict";
import { lstat, readFile } from "node:fs/promises";
import { join } from "node:path";
import { markdownVersionComparison } from "../../benchmarks/zg-retrieval/expansion/compare.mjs";
import { QUALITY_METRICS } from "../../benchmarks/zg-retrieval/reports/ci.mjs";
import { PILOT_SUITES } from "../../benchmarks/zg-retrieval/expansion/config.mjs";

const marker = "<!-- zg-retrieval-pr -->";
const versionMarker = (run) =>
  `<!-- zg-retrieval-run:${run.id}:${run.run_attempt} -->`;
const sameRepo = (left, right) =>
  typeof left === "string" &&
  typeof right === "string" &&
  left.toLowerCase() === right.toLowerCase();
const positiveInteger = (value) => Number.isSafeInteger(value) && value > 0;

function validateRequest(request, run, repository) {
  assert.equal(request.schema_version, 1, "Unsupported PR context schema");
  assert.equal(request.run_id, run.id, "Wrong source run");
  assert.ok(
    positiveInteger(request.run_attempt) &&
      request.run_attempt <= run.run_attempt,
    "Invalid source attempt",
  );
  assert.ok(positiveInteger(request.pr_number), "Invalid PR number");
  for (const side of ["baseline", "candidate"]) {
    assert.match(
      request[side]?.commit ?? "",
      /^[a-f0-9]{40}$/,
      "Invalid commit",
    );
    assert.match(
      request[side]?.repository ?? "",
      /^[\w.-]+\/[\w.-]+$/,
      "Invalid repository",
    );
    assert.ok(
      typeof request[side].ref === "string" && request[side].ref,
      "Missing source ref",
    );
  }
  assert.ok(
    sameRepo(request.baseline.repository, repository),
    "Wrong base repository",
  );
  assert.ok(
    sameRepo(request.candidate.repository, run.head_repository?.full_name),
    "Wrong source repository",
  );
  assert.equal(request.candidate.commit, run.head_sha, "Wrong source commit");
  assert.equal(request.candidate.ref, run.head_branch, "Wrong source branch");
  // When GitHub supplies the original PR association, bind the artifact to it.
  // Some fork runs have an empty pull_requests list; pulls.get below still
  // verifies the PR's current repository, branch and both exact commits.
  if (run.pull_requests?.length) {
    assert.ok(
      run.pull_requests.some(
        (pr) =>
          pr.number === request.pr_number &&
          pr.head.sha === request.candidate.commit &&
          pr.base.sha === request.baseline.commit &&
          pr.head.ref === request.candidate.ref &&
          pr.base.ref === request.baseline.ref,
      ),
      "PR context disagrees with the source run",
    );
  }
}

function currentRequest(pr, request) {
  return (
    pr.state === "open" &&
    ["baseline", "candidate"].every((side) => {
      const current = side === "baseline" ? pr.base : pr.head;
      return (
        sameRepo(current.repo?.full_name, request[side].repository) &&
        current.sha === request[side].commit &&
        current.ref === request[side].ref
      );
    })
  );
}

export function validateComparison(report, request) {
  assert.equal(report.schema_version, 1, "Unsupported comparison schema");
  assert.equal(
    report.kind,
    "retrieval-version-comparison",
    "Wrong report kind",
  );
  assert.equal(report.status, "success", "Incomplete version comparison");
  assert.match(
    report.harness_commit ?? "",
    /^[a-f0-9]{40}$/,
    "Missing harness identity",
  );
  assert.deepEqual(report.errors, [], "Comparison contains errors");
  for (const side of ["baseline", "candidate"]) {
    assert.ok(
      sameRepo(report[side]?.repository, request[side].repository),
      "Wrong report repository",
    );
    assert.equal(
      report[side]?.commit,
      request[side].commit,
      "Wrong report commit",
    );
  }
  assert.ok(
    Array.isArray(report.rows) && report.rows.length === 12,
    "Incomplete suite/mode coverage",
  );
  const rows = [];
  for (const suite of ["sweqa", "beir", "duretrieval", "quarry"]) {
    for (const mode of ["hybrid", "fts", "vector"]) {
      const matches = report.rows.filter(
        (row) => row?.suite === suite && row.mode === mode,
      );
      assert.equal(matches.length, 1, "Missing or duplicated suite/mode");
      const row = matches[0];
      const model =
        suite === "sweqa"
          ? "local/potion-code-16m-v2"
          : PILOT_SUITES[suite].model;
      assert.equal(row.model, model, "Unexpected embedding model");
      const metrics = {};
      for (const metric of QUALITY_METRICS) {
        const cell = row.metrics?.[metric];
        assert.ok(
          [cell?.baseline, cell?.candidate].every(
            (value) => Number.isFinite(value) && value >= 0 && value <= 1,
          ),
          "Invalid quality metric",
        );
        const delta = cell.candidate - cell.baseline;
        assert.ok(
          Number.isFinite(cell.delta) && Math.abs(delta - cell.delta) < 1e-9,
          "Incorrect comparison delta",
        );
        metrics[metric] = {
          baseline: cell.baseline,
          candidate: cell.candidate,
          delta,
        };
      }
      rows.push({ suite, mode, model, metrics });
    }
  }
  // Whitelist data passed to the trusted renderer; artifact Markdown and URLs
  // are never published, interpreted as code, or used to choose the target PR.
  return {
    schema_version: 1,
    kind: report.kind,
    status: "success",
    baseline: {
      repository: request.baseline.repository,
      commit: request.baseline.commit,
    },
    candidate: {
      repository: request.candidate.repository,
      commit: request.candidate.commit,
    },
    harness_commit: report.harness_commit,
    rows,
    errors: [],
  };
}

export function commentBody({ run, request, report, serverUrl, repository }) {
  const runUrl = `${serverUrl}/${repository}/actions/runs/${run.id}/attempts/${run.run_attempt}`;
  let result;
  if (run.conclusion === "success") {
    try {
      result = validateComparison(report, request);
    } catch {
      /* Missing or invalid artifacts remain incomplete. */
    }
  }
  const body = [marker, versionMarker(run), "## ZG Retrieval Benchmark", ""];
  if (result) {
    body.push(
      markdownVersionComparison({
        ...result,
        server_url: serverUrl,
        run_url: runUrl,
      }),
    );
  } else {
    const version = (side) => {
      const { repository: repo, commit } = request[side];
      return `[${repo}@${commit}](${serverUrl}/${repo}/commit/${commit})`;
    };
    body.push(
      "### ❌ Failed / incomplete",
      "",
      `Base: ${version("baseline")}`,
      "",
      `Head: ${version("candidate")}`,
      "",
      `Source workflow conclusion: **${["success", "failure", "timed_out", "action_required", "startup_failure", "stale", "neutral", "skipped"].includes(run.conclusion) ? run.conclusion : "incomplete"}**.`,
      "",
      "No complete validated comparison is available. Missing or invalid results are not zero scores.",
      "",
      `[View run and artifacts](${runUrl})`,
    );
  }
  body.push(
    "",
    "Embedding: local. e2e: skipped. This comment is updated by automatic PR Retrieval runs.",
  );
  return body.join("\n");
}

async function readJson(path) {
  try {
    const info = await lstat(path);
    if (!info.isFile() || info.size > 128 * 1024) return null;
    return JSON.parse(await readFile(path, "utf8"));
  } catch {
    return null;
  }
}

export async function publish({ github, context, core, directory }) {
  assert.equal(
    context.eventName,
    "workflow_run",
    "Only workflow_run can publish automatic results",
  );
  const source = context.payload.workflow_run;
  assert.ok(positiveInteger(source?.id), "Invalid workflow run ID");
  const { data: run } = await github.rest.actions.getWorkflowRun({
    ...context.repo,
    run_id: source.id,
  });
  const { data: workflow } = await github.rest.actions.getWorkflow({
    ...context.repo,
    workflow_id: "retrieval-pr.yml",
  });
  const repository = `${context.repo.owner}/${context.repo.repo}`;
  assert.equal(run.id, source.id, "Wrong workflow run");
  assert.equal(run.workflow_id, workflow.id, "Wrong source workflow");
  assert.equal(
    run.path,
    ".github/workflows/retrieval-pr.yml",
    "Wrong workflow path",
  );
  assert.ok(
    sameRepo(run.repository?.full_name, repository),
    "Wrong run repository",
  );
  assert.equal(
    run.event,
    "pull_request",
    "Only automatic PR runs publish comments",
  );
  assert.ok(positiveInteger(run.run_attempt), "Invalid workflow attempt");
  if (
    run.status !== "completed" ||
    run.conclusion === "cancelled" ||
    run.run_attempt !== source.run_attempt
  ) {
    core.info("Skipping cancelled, superseded or unfinished run.");
    return;
  }
  const request = await readJson(join(directory, "context/request.json"));
  assert.ok(request, "Missing PR context artifact; no PR comment was changed");
  validateRequest(request, run, repository);
  const getPr = async () =>
    (
      await github.rest.pulls.get({
        ...context.repo,
        pull_number: request.pr_number,
      })
    ).data;
  if (!currentRequest(await getPr(), request)) {
    core.info("Skipping results for a closed PR or outdated base/head.");
    return;
  }
  const comments = await github.paginate(github.rest.issues.listComments, {
    ...context.repo,
    issue_number: request.pr_number,
    per_page: 100,
  });
  const previous = comments.find(
    (comment) =>
      comment.user?.login === "github-actions[bot]" &&
      comment.body?.startsWith(marker),
  );
  const version = /<!-- zg-retrieval-run:(\d+):(\d+) -->/.exec(
    previous?.body ?? "",
  );
  if (
    version &&
    (Number(version[1]) > run.id ||
      (Number(version[1]) === run.id && Number(version[2]) > run.run_attempt))
  ) {
    core.info("A newer run already published this PR's results.");
    return;
  }
  const body = commentBody({
    run,
    request,
    report: await readJson(join(directory, "comparison/summary.json")),
    serverUrl: context.serverUrl,
    repository,
  });
  // Recheck after reading artifacts/comments so a push during publication
  // cannot knowingly replace the current PR's result with an older version.
  if (!currentRequest(await getPr(), request)) {
    core.info("PR changed before publication; leaving its comment unchanged.");
    return;
  }
  if (previous)
    await github.rest.issues.updateComment({
      ...context.repo,
      comment_id: previous.id,
      body,
    });
  else
    await github.rest.issues.createComment({
      ...context.repo,
      issue_number: request.pr_number,
      body,
    });
  await core.summary.addRaw(body).write();
}
