import assert from "node:assert/strict";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import {
  INDEX_PREFIX,
  uniqueIndexes,
} from "../../benchmarks/browse-comp-plus-retrieval/identity.mjs";
import { readJson, writeJson } from "../../benchmarks/zg-retrieval/core/io.mjs";

const keyPattern = /^[a-f0-9]{64}$/;
const positive = (value) => {
  const n = Number(value);
  assert.ok(Number.isSafeInteger(n) && n > 0);
  return n;
};

export async function findIndex({ github, context, key, now = Date.now() }) {
  assert.match(key, keyPattern);
  const name = `${INDEX_PREFIX}${key}`;
  const artifacts = await github.paginate(
    github.rest.actions.listArtifactsForRepo,
    { ...context.repo, name, per_page: 100 },
  );
  let expired = false;
  for (const artifact of artifacts
    .filter((a) => a.name === name)
    .sort((a, b) => b.id - a.id)) {
    const { data: run } = await github.rest.actions.getWorkflowRun({
      ...context.repo,
      run_id: artifact.workflow_run.id,
    });
    if (
      run.path?.split("@")[0] !==
        ".github/workflows/browse-comp-plus-retrieval.yml" ||
      !["issue_comment", "workflow_dispatch"].includes(run.event) ||
      run.head_repository?.full_name?.toLowerCase() !==
        `${context.repo.owner}/${context.repo.repo}`.toLowerCase()
    )
      continue;
    if (artifact.expired || Date.parse(artifact.expires_at) <= now) {
      expired = true;
      continue;
    }
    return {
      hit: true,
      key,
      name,
      artifact_id: positive(artifact.id),
      run_id: positive(artifact.workflow_run.id),
      renew: Date.parse(artifact.expires_at) - now < 7 * 86400000,
    };
  }
  assert.ok(
    !expired,
    "The compatible index Artifact has expired. Restore its archived bytes or renew it before expiry; expiry is not a storage change and does not authorize a silent rebuild.",
  );
  // GitHub may delete expired Artifacts from the listing entirely. The bot's
  // durable receipt prevents a deleted known key from becoming a bootstrap miss.
  const comments = await github.paginate(
    github.rest.issues.listCommentsForRepo,
    {
      ...context.repo,
      per_page: 100,
      sort: "created",
      direction: "desc",
    },
  );
  for (const comment of comments) {
    if (
      comment.user?.login !== "github-actions[bot]" ||
      !/^<!-- zg-retrieval-bench:doc:\d+ -->/.test(comment.body ?? "")
    )
      continue;
    const entry = new RegExp(
      `<!-- zg-retrieval-bench:doc-index:${key}:(\\d+):(\\d+) -->`,
    ).exec(comment.body);
    if (!entry) continue;
    const { data: run } = await github.rest.actions.getWorkflowRun({
      ...context.repo,
      run_id: positive(entry[1]),
    });
    if (
      run.path?.split("@")[0] ===
        ".github/workflows/browse-comp-plus-retrieval.yml" &&
      ["issue_comment", "workflow_dispatch"].includes(run.event) &&
      run.head_repository?.full_name?.toLowerCase() ===
        `${context.repo.owner}/${context.repo.repo}`.toLowerCase()
    )
      throw new Error(
        "The registered compatible index Artifact is missing or expired. Restore its archived bytes; this is not a new storage identity.",
      );
  }
  return { hit: false, key, name, renew: false };
}

export async function lookup({ github, context, core, key }) {
  const found = await findIndex({ github, context, key });
  for (const [name, value] of Object.entries(found))
    core.setOutput(name, String(value));
  return found;
}

export async function plan({ directory, request, core }) {
  const identities = [];
  for (const arm of ["baseline", "candidate"]) {
    const identity = await readJson(
      join(directory, `browsecomp-doc-package-${arm}`, "identity.json"),
    );
    assert.equal(identity.builder_commit, request[`${arm}_sha`]);
    assert.match(identity.key, keyPattern);
    assert.equal(identity.artifact_name, `${INDEX_PREFIX}${identity.key}`);
    identities.push({ arm, identity });
  }
  const matrix = { include: uniqueIndexes(identities) };
  core.setOutput("matrix", JSON.stringify(matrix));
  return matrix;
}

export async function receipt({ directory, key, artifactId, runId, output }) {
  const manifest = await readJson(join(directory, "manifest.json"));
  assert.equal(manifest.key, key);
  assert.equal(manifest.build.status, "success");
  const value = {
    schema_version: 1,
    key,
    name: `${INDEX_PREFIX}${key}`,
    artifact_id: positive(artifactId),
    run_id: positive(runId),
  };
  await mkdir(output, { recursive: true });
  await writeJson(join(output, "receipt.json"), value);
  return value;
}

export async function select({ packages, receipts, output, core }) {
  await mkdir(output, { recursive: true });
  for (const arm of ["baseline", "candidate"]) {
    const identity = await readJson(
      join(packages, `browsecomp-doc-package-${arm}`, "identity.json"),
    );
    assert.match(identity.key, keyPattern);
    const value = await readJson(
      join(
        receipts,
        `browsecomp-doc-index-ref-${identity.key}`,
        "receipt.json",
      ),
    );
    assert.equal(value.key, identity.key);
    assert.equal(value.name, `${INDEX_PREFIX}${identity.key}`);
    positive(value.artifact_id);
    positive(value.run_id);
    for (const key of ["name", "artifact_id", "run_id"])
      core.setOutput(`${arm}_${key}`, String(value[key]));
    await writeJson(join(output, `${arm}.json`), value);
  }
}
