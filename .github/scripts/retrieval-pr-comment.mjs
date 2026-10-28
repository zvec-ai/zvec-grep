import assert from "node:assert/strict";
import { lstat, readFile } from "node:fs/promises";
import { join } from "node:path";
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
  assert.match(
    request.harness_commit ?? "",
    /^[a-f0-9]{40}$/,
    "Invalid harness",
  );
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

// Copy numeric aggregates only. Artifact text, raw samples and extra fields must
// never enter the trusted Markdown renderer or become executable instructions.
function performanceSummary(value) {
  assert.equal(value?.schema_version, 1, "Unsupported performance schema");
  const count = (number) => {
    assert.ok(
      Number.isSafeInteger(number) && number >= 0 && number <= 1e9,
      "Invalid performance count",
    );
    return number;
  };
  const stats = (input) => {
    const result = { count: count(input?.count) };
    for (const field of ["mean", "p50", "p95", "p99", "max", "total"]) {
      const number = input[field];
      assert.ok(
        result.count === 0
          ? number === null
          : Number.isFinite(number) && number >= 0,
        "Invalid performance statistic",
      );
      result[field] = number;
    }
    if (result.count) {
      assert.ok(
        result.p50 <= result.p95 &&
          result.p95 <= result.p99 &&
          result.p99 <= result.max &&
          result.mean <= result.max,
        "Inconsistent performance percentiles",
      );
      assert.ok(
        Math.abs(result.total / result.count - result.mean) <=
          Math.max(1, result.mean) * 1e-9,
        "Inconsistent performance total",
      );
    }
    return result;
  };
  const stage = (input, kind) => {
    const result = Object.fromEntries(
      ["planned", "measured", "failed", "memory_measured"].map((key) => [
        key,
        count(input?.[key]),
      ]),
    );
    assert.ok(
      result.failed <= result.measured &&
        result.measured <= result.planned &&
        result.memory_measured <= result.measured,
      "Invalid phase coverage",
    );
    assert.ok(
      Array.isArray(input.concurrency) &&
        input.concurrency.length <= 1 &&
        input.concurrency.every((limit) => limit === 4),
      "Invalid concurrency cap",
    );
    result.concurrency = [...input.concurrency];
    result.total_ms = stats(input.total_ms);
    result.memory_rss_bytes = stats(input.memory_rss_bytes);
    assert.ok(
      result.total_ms.count <= result.measured,
      "Invalid time coverage",
    );
    assert.ok(
      !result.memory_measured || result.memory_rss_bytes.count > 0,
      "Missing RSS samples",
    );
    const field = kind === "index" ? "index_bytes" : "latency_ms";
    result[field] = stats(input[field]);
    if (kind === "index")
      assert.ok(
        result.index_bytes.count <= result.measured,
        "Invalid index coverage",
      );
    return result;
  };
  return {
    schema_version: 1,
    percentile_method: "p50 median; p95/p99 nearest-rank",
    index: stage(value.index, "index"),
    query: Object.fromEntries(
      ["hybrid", "fts", "vector"].map((mode) => [
        mode,
        stage(value.query?.[mode], "query"),
      ]),
    ),
  };
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
  assert.equal(
    report.harness_commit,
    request.harness_commit,
    "Wrong harness commit",
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
  const performance = {};
  for (const suite of ["sweqa", "beir", "duretrieval", "quarry"]) {
    performance[suite] = Object.fromEntries(
      ["baseline", "candidate"].map((side) => [
        side,
        performanceSummary(report.performance?.[suite]?.[side]),
      ]),
    );
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
  const package_sha256 = Object.fromEntries(
    ["baseline", "candidate"].map((side) => {
      const hash = report.package_sha256?.[side];
      assert.match(hash ?? "", /^[a-f0-9]{64}$/, "Invalid package hash");
      return [side, hash];
    }),
  );
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
    package_sha256,
    performance,
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
      markdownPrComparison({
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

export function markdownPrComparison(result) {
  const suites = ["sweqa", "beir", "duretrieval", "quarry"];
  const modes = ["hybrid", "fts", "vector"];
  const sides = ["baseline", "candidate"];
  const label = (suite) =>
    ({
      sweqa: "SWE-QA20",
      beir: "BEIR",
      duretrieval: "DuRetrieval",
      quarry: "Quarry",
    })[suite];
  const number = (value, unit = 1) =>
    Number.isFinite(value) ? (value / unit).toFixed(2) : "—";
  const stats = (value, fields, unit = 1) =>
    fields.map((field) => number(value[field], unit)).join(" / ");
  const pair = (suite, select) =>
    sides.map((side) => select(result.performance[suite][side])).join(" → ");
  const version = (side) => {
    const { repository, commit } = result[side];
    return `[\`${commit.slice(0, 7)}\`](${result.server_url}/${repository}/commit/${commit})`;
  };
  const mib = 1024 ** 2;
  const lines = [
    `**✅ Complete** · Base ${version("baseline")} → Head ${version("candidate")} · [Run & artifacts](${result.run_url})`,
    "",
    "Cells show **base → head**. — = unavailable. Concurrency: **≤4**.",
    "",
    "### 1 · Index",
    "",
    "| Suite | Total time (s) | Peak RSS (MiB) | Index size (MiB) |",
    "| --- | ---: | ---: | ---: |",
    ...suites.map(
      (suite) =>
        `| ${label(suite)} | ${pair(suite, (p) => number(p.index.total_ms.total, 1000))} | ${pair(suite, (p) => number(p.index.memory_rss_bytes.max, mib))} | ${pair(suite, (p) => number(p.index.index_bytes.total, mib))} |`,
    ),
    "",
    "### 2 · Query",
    "",
    "| Suite / mode | Mean RT (ms) | p95 RT (ms) | Peak RSS (MiB) |",
    "| --- | ---: | ---: | ---: |",
    ...suites.flatMap((suite) =>
      modes.map(
        (mode) =>
          `| ${label(suite)} / ${mode} | ${pair(suite, (p) => number(p.query[mode].latency_ms.mean))} | ${pair(suite, (p) => number(p.query[mode].latency_ms.p95))} | ${pair(suite, (p) => number(p.query[mode].memory_rss_bytes.max, mib))} |`,
      ),
    ),
    "",
    "<details>",
    "<summary>Full performance · sample counts, all percentiles & group totals</summary>",
    "",
    "Σ = sum; μ = mean. p50 uses the median; p95/p99 use nearest rank. Peak RSS is the sampled maximum. Each row below is one version.",
    "",
    "**Index**",
    "",
    "| Suite / version | Phases measured/planned; failed | Time n; Σ / μ / p95 / p99 (s) | RSS n; μ / p95 / p99 / peak (MiB) | Index n; Σ / μ / p95 / p99 (MiB) |",
    "| --- | --- | --- | --- | --- |",
    ...suites.flatMap((suite) =>
      sides.map((side) => {
        const stage = result.performance[suite][side].index;
        return `| ${label(suite)} / ${side === "baseline" ? "base" : "head"} | ${stage.measured}/${stage.planned}; ${stage.failed} | ${stage.total_ms.count}; ${stats(stage.total_ms, ["total", "mean", "p95", "p99"], 1000)} | ${stage.memory_rss_bytes.count} (${stage.memory_measured}/${stage.planned} complete phases); ${stats(stage.memory_rss_bytes, ["mean", "p95", "p99", "max"], mib)} | ${stage.index_bytes.count}/${stage.planned}; ${stats(stage.index_bytes, ["total", "mean", "p95", "p99"], mib)} |`;
      }),
    ),
    "",
    "**Query**",
    "",
    "| Suite / mode / version | RT n; mean / p50 / p95 / p99 (ms) | Group phases measured/planned; failed; time n; Σ / μ / p95 / p99 (s) | RSS n; μ / p95 / p99 / peak (MiB) |",
    "| --- | --- | --- | --- |",
    ...suites.flatMap((suite) =>
      modes.flatMap((mode) =>
        sides.map((side) => {
          const stage = result.performance[suite][side].query[mode];
          return `| ${label(suite)} / ${mode} / ${side === "baseline" ? "base" : "head"} | ${stage.latency_ms.count}; ${stats(stage.latency_ms, ["mean", "p50", "p95", "p99"])} | ${stage.measured}/${stage.planned}; ${stage.failed}; ${stage.total_ms.count}; ${stats(stage.total_ms, ["total", "mean", "p95", "p99"], 1000)} | ${stage.memory_rss_bytes.count} (${stage.memory_measured}/${stage.planned} complete phases); ${stats(stage.memory_rss_bytes, ["mean", "p95", "p99", "max"], mib)} |`;
        }),
      ),
    ),
    "",
    "</details>",
    "",
    "### 3 · Retrieval quality",
    "",
    "<details>",
    "<summary>Quality scores · 4 suites × 3 modes</summary>",
    "",
    "Each cell is base / head / Δ. Δ = head − base; higher is better. Quality is report-only.",
    "",
    "| Suite | Mode | File Hit@1 | File Hit@5 | File Hit@10 | MRR@10 | nDCG@10 |",
    "| --- | --- | ---: | ---: | ---: | ---: | ---: |",
    ...result.rows.map(
      (row) =>
        `| ${label(row.suite)} | ${row.mode} | ${QUALITY_METRICS.map(
          (metric) => {
            const cell = row.metrics[metric];
            return `${cell.baseline.toFixed(4)} / ${cell.candidate.toFixed(4)} / ${cell.delta > 0 ? "+" : ""}${cell.delta.toFixed(4)}`;
          },
        ).join(" | ")} |`,
    ),
    "",
    "Same frozen queries, gold labels, local models and harness; fresh indexes and five calls per query/mode. SWE-QA nDCG uses repository macro averaging; other suites use query means.",
    "",
    "</details>",
    "",
    "<details>",
    "<summary>Build identities & measurement notes</summary>",
    "",
    `Harness: \`${result.harness_commit}\``,
    "",
    ...sides.map(
      (side) =>
        `${side === "baseline" ? "Base" : "Head"}: \`${result[side].commit}\` · package SHA-256: \`${result.package_sha256[side]}\``,
    ),
    "",
    "Index totals include startup, model loading/download and optimization. Query RT includes attempted public calls; group time includes response capture. Phase sums are not parallel workflow wall time. Fixed mode order and first-call/cache effects apply. RSS samples product process trees and resident daemons every 100 ms, excludes the driver, may double-count shared pages and miss short peaks. Index size sums regular files under .zvec-grep; model caches and symlinks are excluded. Missing values and coverage are explicit; raw samples remain in the run artifacts.",
    "",
    "</details>",
  ];
  return `${lines.join("\n")}\n`;
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
