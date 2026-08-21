import assert from "node:assert/strict";
import { realpathSync } from "node:fs";
import {
  access,
  mkdir,
  mkdtemp,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import {
  resetWorkspaceIndex,
  workspaceIndexLocation,
} from "../../dist/engine/service/root.js";

test("workspace index locations resolve an existing index symlink", async () => {
  const temporaryRoot = await mkdtemp(join(tmpdir(), "zvec-grep-root-"));

  try {
    const corpusRoot = join(temporaryRoot, "corpus");
    const indexHome = join(corpusRoot, ".zvec-grep");
    const workspaceRoot = join(temporaryRoot, "workspace");
    await mkdir(indexHome, { recursive: true });
    await mkdir(workspaceRoot, { recursive: true });
    await symlink(indexHome, join(workspaceRoot, ".zvec-grep"));

    const canonicalHome = realpathSync(indexHome);
    const canonicalRoot = dirname(canonicalHome);
    assert.deepEqual(workspaceIndexLocation(workspaceRoot), {
      root: canonicalRoot,
      home: canonicalHome,
      manifestPath: join(canonicalHome, "manifest.json"),
      indexPath: join(canonicalHome, "index.zvec"),
    });
  } finally {
    await rm(temporaryRoot, { recursive: true, force: true });
  }
});

test("resetWorkspaceIndex removes vector, manifest, and graph data", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zvec-grep-reset-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const location = workspaceIndexLocation(root);
  await mkdir(join(location.home, "code-graph"), { recursive: true });
  await writeFile(location.manifestPath, "{}");
  await writeFile(location.indexPath, "index");
  await writeFile(join(location.home, "files.zvec"), "files");
  await writeFile(join(location.home, "code-graph", "graph.sqlite"), "graph");

  resetWorkspaceIndex(location);

  for (const path of [
    location.manifestPath,
    location.indexPath,
    join(location.home, "files.zvec"),
    join(location.home, "code-graph"),
  ]) {
    await assert.rejects(access(path), { code: "ENOENT" });
  }
});
