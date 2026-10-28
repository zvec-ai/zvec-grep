import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { parseArgs } from "node:util";
import { pathToFileURL } from "node:url";
import {
  fileHash,
  objectHash,
  readJson,
  run,
  sha256,
  writeJson,
} from "../zg-retrieval/core/io.mjs";

const SHA = /^[a-f0-9]{40}$/;
export const INDEX_PREFIX = "browsecomp-doc-index-";

/** Query code, PR SHA and harness SHA deliberately do not invalidate an index. */
export async function indexIdentity(source, lock) {
  source = resolve(source);
  const commit = (
    await run("git", ["-C", source, "rev-parse", "HEAD"])
  ).stdout.trim();
  assert.match(commit, SHA);
  const workspace = await readFile(
    join(source, "rust/crates/zg-engine/src/workspace/mod.rs"),
    "utf8",
  );
  const version = /CURRENT_INDEX_VERSION:\s*u32\s*=\s*(\d+)/.exec(workspace);
  assert.ok(version, "candidate has no explicit persisted index version");
  const tracked = (
    await run("git", [
      "-C",
      source,
      "ls-files",
      "-z",
      "rust/crates/zg-engine/src/storage",
      "rust/crates/zg-engine/src/workspace",
    ])
  ).stdout
    .split("\0")
    .filter(Boolean);
  const files = [];
  for (const path of tracked.sort()) {
    if (
      !path.endsWith(".rs") ||
      /(?:\/tests\/|\/tests\.rs$|_tests\.rs$)/.test(path)
    )
      continue;
    const text = (await readFile(join(source, path), "utf8")).split(
      /\n#\[cfg\(test\)\]\s*\nmod tests\b/,
    )[0];
    files.push({ path, sha256: sha256(text) });
  }
  assert.ok(files.length > 0);
  const cargo = await readFile(join(source, "rust/Cargo.lock"), "utf8");
  const dependencies = cargo
    .split("[[package]]")
    .filter((block) =>
      /^\s*name = "(?:zvec-rust(?:-sys|-build)?|rusqlite|libsqlite3-sys)"/m.test(
        block,
      ),
    )
    .map((block) => block.trim())
    .sort();
  assert.ok(
    dependencies.length >= 3,
    "missing underlying storage dependency identities",
  );
  const storage = { index_version: Number(version[1]), files, dependencies };
  const inputs = {
    corpus: lock.corpus,
    model: lock.model,
    model_artifacts_sha256: lock.model_artifacts_sha256,
    device: lock.device,
    max_file_size: lock.index_max_filesize,
    glob: "*.md",
    platform: "linux-x64",
  };
  const key = objectHash({ schema_version: 1, storage, inputs });
  return {
    schema_version: 1,
    key,
    artifact_name: `${INDEX_PREFIX}${key}`,
    storage,
    inputs,
    builder_commit: commit,
  };
}

export function uniqueIndexes(identities) {
  const seen = new Set();
  return identities
    .filter(({ identity }) => {
      if (seen.has(identity.key)) return false;
      seen.add(identity.key);
      return true;
    })
    .map(({ arm, identity }) => ({
      arm,
      key: identity.key,
      artifact_name: identity.artifact_name,
    }));
}

export async function main(args = process.argv.slice(2)) {
  const { values } = parseArgs({
    args,
    options: Object.fromEntries(
      ["source", "lock", "output", "commit"].map((key) => [
        key,
        { type: "string" },
      ]),
    ),
  });
  const identity = await indexIdentity(
    values.source,
    await readJson(values.lock),
  );
  if (values.commit) assert.equal(identity.builder_commit, values.commit);
  identity.source_lock_sha256 = await fileHash(values.lock);
  await writeJson(values.output, identity);
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
  await main();
