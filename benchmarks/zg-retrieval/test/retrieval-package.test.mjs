import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  downloadNodePackage,
  NODE_RELEASE,
  verifyRetrievalPackage,
} from "../../shared/retrieval-package.mjs";

test("Node.js release downloads reject HTTP errors and substituted package bytes", async (t) => {
  const output = await mkdtemp(join(tmpdir(), "zg-node-package-"));
  t.after(() => rm(output, { recursive: true, force: true }));
  await assert.rejects(
    downloadNodePackage({
      output,
      fetcher: async () => new Response("missing", { status: 404 }),
    }),
    /download failed: 404/,
  );
  await assert.rejects(
    downloadNodePackage({
      output,
      fetcher: async (url) => {
        assert.equal(url, NODE_RELEASE.url);
        return new Response("substituted tarball");
      },
    }),
    /differs from the pinned official release/,
  );
});

test("Node.js cache validation checks pinned bytes even when its manifest looks valid", async (t) => {
  const directory = await mkdtemp(join(tmpdir(), "zg-node-cache-"));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const manifest = {
    schema_version: 1,
    runtime: "node-0.2.2",
    candidate_commit: NODE_RELEASE.identity,
    tarball: "candidate.tgz",
    tarball_sha256: NODE_RELEASE.sha256,
    registry_url: NODE_RELEASE.url,
    registry_integrity: NODE_RELEASE.integrity,
  };
  await writeFile(join(directory, "manifest.json"), JSON.stringify(manifest));
  await writeFile(join(directory, "candidate.tgz"), "substituted tarball");
  await assert.rejects(
    verifyRetrievalPackage({ directory, commit: NODE_RELEASE.identity }),
    /differs from the pinned official release/,
  );
  await assert.rejects(
    verifyRetrievalPackage({ directory, commit: "npm:@zvec/zvec-grep@0.2.3" }),
    /expected candidate commit must be a full SHA/,
  );
  await assert.rejects(
    verifyRetrievalPackage({ directory, commit: "a".repeat(40) }),
    /does not match the selected source/,
  );
});
