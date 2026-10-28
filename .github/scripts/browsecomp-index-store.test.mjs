import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  findIndex,
  readIndexReceipts,
  select,
} from "./browsecomp-index-store.mjs";

const key = "a".repeat(64),
  now = Date.parse("2026-10-28T00:00:00Z");
function fixture(
  artifacts = [],
  run = {
    path: ".github/workflows/browse-comp-plus-retrieval.yml",
    event: "issue_comment",
    head_repository: { full_name: "owner/repo" },
  },
) {
  const listArtifactsForRepo = () => {},
    listCommentsForRepo = () => {};
  return {
    context: { repo: { owner: "owner", repo: "repo" } },
    key,
    now,
    github: {
      paginate: async (method) =>
        method === listArtifactsForRepo ? artifacts : [],
      rest: {
        issues: { listCommentsForRepo },
        actions: {
          listArtifactsForRepo,
          getWorkflowRun: async () => ({ data: run }),
        },
      },
    },
  };
}
const cached = (id = 12) => ({
  id,
  name: `browsecomp-doc-index-${key}`,
  workflow_run: { id: 123 },
  expired: false,
  expires_at: "2027-01-01T00:00:00Z",
});

test("an existing storage identity is reused, independently of PR SHA", async () => {
  const found = await findIndex(fixture([cached()]));
  assert.equal(found.hit, true);
  assert.equal(found.artifact_id, 12);
  assert.equal(found.renew, false);
  assert.equal((await findIndex(fixture([]))).hit, false);
});
test("an expired or deleted known key cannot be mistaken for a first build", async () => {
  const x = fixture();
  const comment = {
    user: { login: "github-actions[bot]" },
    body: `<!-- zg-retrieval-bench:doc:123 -->\n<!-- zg-retrieval-bench:doc-index:${key}:123:12 -->`,
  };
  x.github.paginate = async (method) =>
    method === x.github.rest.issues.listCommentsForRepo ? [comment] : [];
  await assert.rejects(findIndex(x), /missing or expired/);
  comment.user.login = "contributor";
  assert.equal((await findIndex(x)).hit, false);
});
test("foreign workflows and PR-run artifacts cannot poison the index catalog", async () => {
  for (const run of [
    {
      path: ".github/workflows/ci.yml",
      event: "workflow_dispatch",
      head_repository: { full_name: "owner/repo" },
    },
    {
      path: ".github/workflows/browse-comp-plus-retrieval.yml",
      event: "pull_request",
      head_repository: { full_name: "owner/repo" },
    },
    {
      path: ".github/workflows/browse-comp-plus-retrieval.yml",
      event: "issue_comment",
      head_repository: { full_name: "other/repo" },
    },
  ])
    assert.equal((await findIndex(fixture([cached()], run))).hit, false);
});
test("near-expiry Artifacts renew identical bytes; expiration or API failure never falls back to reindex", async () => {
  const value = cached();
  value.expires_at = "2026-10-30T00:00:00Z";
  const found = await findIndex(fixture([value]));
  assert.equal(found.hit, true);
  assert.equal(found.renew, true);
  value.expired = true;
  await assert.rejects(findIndex(fixture([value])), /expired/);
  const x = fixture();
  x.github.paginate = async () => {
    throw new Error("API unavailable");
  };
  await assert.rejects(findIndex(x), /API unavailable/);
});

test("v8 flattened single receipts and nested multiple receipts select the exact requested Artifacts", async (t) => {
  for (const shared of [true, false]) {
    await t.test(
      shared ? "single flattened Artifact" : "two named Artifacts",
      async (t) => {
        const directory = await mkdtemp(join(tmpdir(), "browsecomp-receipts-"));
        t.after(() => rm(directory, { recursive: true, force: true }));
        const packages = join(directory, "packages"),
          receipts = join(directory, "receipts"),
          output = join(directory, "selected");
        const outputs = {};
        for (const [index, arm] of ["baseline", "candidate"].entries()) {
          const armKey = shared || index === 0 ? key : "b".repeat(64);
          const pkg = join(packages, `browsecomp-doc-package-${arm}`);
          await mkdir(pkg, { recursive: true });
          await writeFile(
            join(pkg, "identity.json"),
            JSON.stringify({ key: armKey }),
          );
          if (shared && index === 1) continue;
          const target = shared
            ? receipts
            : join(receipts, `browsecomp-doc-index-ref-${armKey}`);
          await mkdir(target, { recursive: true });
          await writeFile(
            join(target, "receipt.json"),
            JSON.stringify({
              key: armKey,
              name: `browsecomp-doc-index-${armKey}`,
              artifact_id: 12 + index,
              run_id: 123,
            }),
          );
        }
        await select({
          packages,
          receipts,
          output,
          core: { setOutput: (k, v) => (outputs[k] = v) },
        });
        assert.equal(outputs.baseline_artifact_id, "12");
        assert.equal(outputs.candidate_artifact_id, shared ? "12" : "13");
        assert.equal(
          JSON.parse(await readFile(join(output, "candidate.json"), "utf8"))
            .artifact_id,
          shared ? 12 : 13,
        );
        await writeFile(
          join(packages, "browsecomp-doc-package-candidate/identity.json"),
          JSON.stringify({ key: "c".repeat(64) }),
        );
        await assert.rejects(
          select({ packages, receipts, output, core: { setOutput() {} } }),
          /missing receipt/,
        );
      },
    );
  }
});

test("ambiguous or misnamed receipt layouts fail closed", async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "browsecomp-receipt-layout-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const value = {
    key,
    name: `browsecomp-doc-index-${key}`,
    artifact_id: 12,
    run_id: 123,
  };
  const nested = join(directory, `browsecomp-doc-index-ref-${"b".repeat(64)}`);
  await mkdir(nested);
  await writeFile(join(nested, "receipt.json"), JSON.stringify(value));
  await assert.rejects(readIndexReceipts(directory));
  await writeFile(join(directory, "receipt.json"), JSON.stringify(value));
  await assert.rejects(readIndexReceipts(directory), /ambiguous/);
});
