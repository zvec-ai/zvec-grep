import assert from "node:assert/strict";
import { join, resolve } from "node:path";
import { readJson, run } from "../zg-retrieval/core/io.mjs";

/** Workspace runtime settings take precedence over the model-cache environment. */
export async function verifyModelCacheLocation(root, model, cache) {
  const manifest = await readJson(join(root, ".zvec-grep/manifest.json"));
  assert.equal(
    manifest.embeddingRuntimes?.[model]?.cacheDir,
    resolve(cache),
    "restore exact model bytes at the persisted cache path before querying",
  );
}

export function candidateCommand(candidate, args) {
  return { command: candidate.cli, args };
}

export function runCandidate(candidate, args, options) {
  return run(candidate.cli, args, options);
}
