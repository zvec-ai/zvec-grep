import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import { join } from "node:path";

// A whole command on its own line; mentions in prose, quotes and code do not run.
export function isBenchCommand(body) {
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
      return !fence && /^ {0,3}@zg-bench[ \t]*$/.test(line);
    });
}

export async function authorize({ github, context, env }) {
  const actors = new Set([context.actor, env.BENCH_TRIGGERING_ACTOR]);
  for (const username of actors) {
    assert.ok(username, "Missing original or rerun actor");
    const { data } = await github.rest.repos.getCollaboratorPermissionLevel({
      ...context.repo,
      username,
    });
    assert.ok(
      ["admin", "maintain"].includes(data.role_name),
      `${username} requires the admin or maintain repository role`,
    );
  }
}

const marker = (context) => `<!-- zg-bench:run:${context.runId} -->`;
const runUrl = (context) =>
  `${context.serverUrl}/${context.repo.owner}/${context.repo.repo}/actions/runs/${context.runId}`;
const commitLink = (context, repository, sha) =>
  `[${repository}@${sha}](${context.serverUrl}/${repository}/commit/${sha})`;

async function upsert({ github, context, prNumber, commentId, body }) {
  // Locate our own marker on reruns. A user's copied marker cannot be updated.
  const comments = await github.paginate(github.rest.issues.listComments, {
    ...context.repo,
    issue_number: prNumber,
    per_page: 100,
  });
  const previous = comments.find(
    (comment) =>
      comment.user?.login === "github-actions[bot]" &&
      comment.body?.startsWith(marker(context)) &&
      (!commentId || String(comment.id) === String(commentId)),
  );
  const { data } = previous
    ? await github.rest.issues.updateComment({
        ...context.repo,
        comment_id: previous.id,
        body,
      })
    : await github.rest.issues.createComment({
        ...context.repo,
        issue_number: prNumber,
        body,
      });
  return data.id;
}

export async function prepare({ github, context, core, env }) {
  core.setOutput("accepted", "false");
  if (context.eventName === "issue_comment") {
    const { issue, comment, action } = context.payload;
    if (
      action !== "created" ||
      !issue?.pull_request ||
      comment?.user?.type !== "User" ||
      comment.user.login !== context.actor ||
      !isBenchCommand(comment.body)
    )
      return;
  } else {
    assert.equal(context.eventName, "workflow_dispatch", "Unsupported event");
  }
  await authorize({ github, context, env });
  const rawNumber =
    context.eventName === "issue_comment"
      ? String(context.payload.issue.number)
      : env.PR_NUMBER;
  assert.match(rawNumber ?? "", /^[1-9]\d*$/, "Invalid PR number");
  const prNumber = Number(rawNumber);
  assert.ok(Number.isSafeInteger(prNumber), "Invalid PR number");
  const { data: pr } = await github.rest.pulls.get({
    ...context.repo,
    pull_number: prNumber,
  });
  assert.equal(pr.state, "open", "Benchmark requires an open PR");
  assert.equal(
    pr.base.repo.full_name.toLowerCase(),
    `${context.repo.owner}/${context.repo.repo}`.toLowerCase(),
    "PR belongs to another repository",
  );
  assert.match(pr.head.sha, /^[a-f0-9]{40}$/, "Invalid PR head SHA");
  assert.match(pr.base.sha, /^[a-f0-9]{40}$/, "Invalid PR base SHA");
  assert.match(
    pr.head.repo?.full_name ?? "",
    /^[\w.-]+\/[\w.-]+$/,
    "PR source repository is unavailable",
  );
  const runE2e =
    context.eventName === "workflow_dispatch" && env.RUN_E2E === "true";
  const trialsPerProfile = runE2e
    ? JSON.parse(
        await readFile(
          new URL("../../benchmarks/swe-qa-bench/ci-config.json", import.meta.url),
          "utf8",
        ),
      ).trials_per_profile
    : null;
  const body = [
    marker(context),
    "## ZG Benchmark · running",
    "",
    `Base: ${commitLink(context, pr.base.repo.full_name, pr.base.sha)}`,
    "",
    `Head: ${commitLink(context, pr.head.repo.full_name, pr.head.sha)}`,
    "",
    "| Benchmark | Status | Configuration |",
    "| --- | --- | --- |",
    "| Retrieval | Queued | PR base vs head; SWE-QA20 + BEIR + DuRetrieval + Quarry; local embedding |",
    runE2e
      ? `| e2e | Queued | SWE-QA20; OpenCode + GLM-5.2; baseline / zvec-grep; ${trialsPerProfile} trials per profile; trim one lowest and one highest score per task/profile |`
      : "| e2e | Skipped | Disabled by default |",
    "",
    `[View run and artifacts](${runUrl(context)})`,
    "",
    "This comment will be updated with the results. Both commits are pinned for this run; a later push does not change either version.",
  ].join("\n");
  const commentId = await upsert({ github, context, prNumber, body });
  for (const [name, value] of Object.entries({
    candidate_sha: pr.head.sha,
    candidate_repository: pr.head.repo.full_name,
    baseline_sha: pr.base.sha,
    baseline_repository: pr.base.repo.full_name,
    run_e2e: String(runE2e),
    pr_number: prNumber,
    comment_id: commentId,
    accepted: "true",
  }))
    core.setOutput(name, String(value));
}

async function loadReport(directory, jsonFile, mdFile, identityFile) {
  try {
    return {
      json: JSON.parse(await readFile(join(directory, jsonFile), "utf8")),
      markdown: await readFile(join(directory, mdFile), "utf8"),
      identity: identityFile
        ? JSON.parse(await readFile(join(directory, identityFile), "utf8"))
        : null,
    };
  } catch {
    return null;
  }
}

export async function loadReports(root, runId, candidateSha) {
  const retrieval = await loadReport(
    join(root, "retrieval"),
    "summary.json",
    "summary.md",
  );
  let directories = [];
  try {
    directories = await readdir(join(root, "e2e"));
  } catch {
    // A failed run may produce no report artifacts.
  }
  const prefix = `swe-qa-aggregate-report-${runId}-`;
  const attempts = directories
    .filter(
      (name) =>
        name.startsWith(prefix) && /^\d+$/.test(name.slice(prefix.length)),
    )
    .sort(
      (a, b) => Number(b.slice(prefix.length)) - Number(a.slice(prefix.length)),
    );
  let e2e = null;
  for (const name of attempts) {
    const report = await loadReport(
      join(root, "e2e", name),
      "report.json",
      "report.md",
      "candidate-manifest.json",
    );
    if (report?.identity?.candidate_commit === candidateSha) {
      e2e = { ...report, artifact_attempt: Number(name.slice(prefix.length)) };
      break;
    }
  }
  // download-artifact extracts a single matching artifact directly into path,
  // even when it was selected by pattern and merge-multiple is false.
  if (!e2e) {
    const report = await loadReport(
      join(root, "e2e"),
      "report.json",
      "report.md",
      "candidate-manifest.json",
    );
    if (report?.identity?.candidate_commit === candidateSha) e2e = report;
  }
  return { retrieval, e2e };
}

function reportSection(name, jobResult, report, sha) {
  const identity =
    name === "Retrieval"
      ? report?.json?.candidate?.commit
      : report?.identity?.candidate_commit;
  if (!report || identity !== sha || !report.markdown.trim())
    return {
      complete: false,
      body: `### ${name} · ❌ Failed / incomplete (job: ${jobResult})\n\nNo report for this candidate was produced. Missing results are not zero scores.`,
    };
  const complete =
    jobResult === "success" &&
    (name === "Retrieval"
      ? report.json.status === "success"
      : [2, 3].includes(report.json.schema_version) &&
        report.json.gate?.passed === true &&
        Array.isArray(report.json.cases) &&
        report.json.cases.length === 20 &&
        new Set(report.json.cases.map((row) => row.task_id)).size === 20 &&
        Array.isArray(report.json.gate?.missing_tasks) &&
        report.json.gate.missing_tasks.length === 0);
  // Leave room for both reports under GitHub's comment size limit. The complete
  // Markdown and machine-readable evidence remain in the linked artifacts.
  const markdown = report.markdown.replace(
    /(^|[^\w])@(?=[a-z\d])/gi,
    "$1@\u200b",
  );
  const excerpt =
    markdown.length > 24000
      ? `${markdown.slice(0, 24000)}\n\n_Report truncated; see the full artifact._`
      : markdown;
  return {
    complete,
    body: `### ${name} · ${complete ? "✅ Complete" : "❌ Failed / incomplete"}\n\n${report.artifact_attempt ? `Report artifact: run attempt ${report.artifact_attempt}.\n\n` : ""}${excerpt}`,
  };
}

export function resultComment({
  context,
  request,
  results,
  retrieval,
  e2e,
  currentSha,
  currentBaseSha,
}) {
  if (
    request.baseline_sha &&
    (retrieval?.json?.kind !== "retrieval-version-comparison" ||
      retrieval?.json?.baseline?.commit !== request.baseline_sha ||
      retrieval?.json?.baseline?.repository !== request.baseline_repository ||
      retrieval?.json?.candidate?.repository !== request.candidate_repository)
  )
    retrieval = null;
  const sections = [
    reportSection(
      "Retrieval",
      results.retrieval,
      retrieval,
      request.candidate_sha,
    ),
    request.run_e2e === "false"
      ? { complete: true, body: "### e2e · Skipped\n\nDisabled for this run." }
      : reportSection("e2e", results.e2e, e2e, request.candidate_sha),
  ];
  const complete = sections.every((section) => section.complete);
  return {
    complete,
    body: [
      marker(context),
      `## ZG Benchmark · ${complete ? "✅ Complete" : "❌ Failed / incomplete"}`,
      "",
      ...(request.baseline_sha
        ? [
            `Base: ${commitLink(context, request.baseline_repository, request.baseline_sha)}`,
            "",
          ]
        : []),
      `Head: ${commitLink(context, request.candidate_repository, request.candidate_sha)}`,
      "",
      ...((currentSha && currentSha !== request.candidate_sha) ||
      (currentBaseSha && currentBaseSha !== request.baseline_sha)
        ? [
            "⚠️ The PR has newer commits. These results apply only to the pinned versions above; comment `@zg-bench` again to test the current base and head.",
            "",
          ]
        : []),
      `[View run and full artifacts](${runUrl(context)})`,
      "",
      ...sections.flatMap((section) => [section.body, ""]),
    ].join("\n"),
  };
}

export async function finish({ github, context, core, env }) {
  await authorize({ github, context, env });
  const needs = JSON.parse(env.BENCH_NEEDS);
  const request = needs.request.outputs;
  assert.equal(request.accepted, "true");
  assert.match(request.candidate_sha, /^[a-f0-9]{40}$/);
  const reports = await loadReports(
    env.BENCH_REPORTS,
    context.runId,
    request.candidate_sha,
  );
  const { data: pr } = await github.rest.pulls.get({
    ...context.repo,
    pull_number: Number(request.pr_number),
  });
  const result = resultComment({
    context,
    request,
    results: { retrieval: needs.retrieval.result, e2e: needs.e2e.result },
    ...reports,
    currentSha: pr.head.sha,
    currentBaseSha: pr.base.sha,
  });
  await upsert({
    github,
    context,
    prNumber: Number(request.pr_number),
    commentId: request.comment_id,
    body: result.body,
  });
  await core.summary.addRaw(result.body).write();
  if (!result.complete)
    core.setFailed(
      "Benchmark failed or reports are incomplete; see the PR comment.",
    );
}
