import assert from "node:assert/strict";
import fs from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import {
  mkdir,
  mkdtemp,
  realpath,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, sep } from "node:path";
import test from "node:test";
import { createCanonicalPathResolver } from "../../dist/engine/utils/canonical-path.js";
import { resolveWorkspaceFilePath } from "../../dist/engine/pipeline/indexing/root-paths.js";

async function fixture(t, simulateShortName) {
  const base = await mkdtemp(join(tmpdir(), "zg-watch-native-"));
  t.after(() => rm(base, { recursive: true, force: true }));
  const root = join(base, "workspace");
  const alias = join(base, "SHORT~1");
  await mkdir(root);
  await writeFile(join(root, "changed.md"), "changed content");
  await symlink(root, alias, process.platform === "win32" ? "junction" : "dir");
  const physical = await realpath(root);
  if (simulateShortName) {
    const original = fs.realpathSync;
    const jsRealpath = (input, ...rest) => {
      const resolved = original(input, ...rest);
      // Windows' JS resolver can retain an existing 8.3 alias. Missing
      // paths still throw; native resolution and all I/O remain real.
      return input === alias || input.startsWith(`${alias}${sep}`)
        ? input
        : resolved;
    };
    jsRealpath.native = original.native;
    t.mock.method(fs, "realpathSync", jsRealpath);
    syncBuiltinESMExports();
    t.after(() => {
      t.mock.restoreAll();
      syncBuiltinESMExports();
    });
  }
  return {
    root,
    alias,
    physical,
    resolver: createCanonicalPathResolver(physical),
  };
}

for (const [label, suffix] of [
  ["changed file", "changed.md"],
  ["deleted file", "deleted.md"],
  ["workspace root", ""],
]) {
  test(`F1 native alias mapping handles a ${label} notification`, async (t) => {
    const f = await fixture(t, true);
    assert.equal(
      resolveWorkspaceFilePath(join(f.alias, suffix), f.resolver),
      join(f.physical, suffix),
    );
  });
}

test("F1 native alias mapping retains internal link identity and rejects escape", async (t) => {
  const f = await fixture(t, true);
  const kind = process.platform === "win32" ? "junction" : "dir";
  await symlink(f.root, join(f.root, "self"), kind);
  await symlink(tmpdir(), join(f.root, "escape"), kind);
  assert.equal(
    resolveWorkspaceFilePath(join(f.alias, "self", "changed.md"), f.resolver),
    join(f.physical, "self", "changed.md"),
  );
  const outside = join(f.alias, "escape", "missing.md");
  assert.equal(resolveWorkspaceFilePath(outside, f.resolver), outside);
});

test("F1 real temporary watcher paths map to the daemon's native workspace", async (t) => {
  const f = await fixture(t, false);
  // On hosted Windows, tmpdir uses RUNNER~1 while realpath expands it.
  assert.equal(
    resolveWorkspaceFilePath(join(f.root, "changed.md"), f.resolver),
    join(f.physical, "changed.md"),
  );
  assert.equal(
    resolveWorkspaceFilePath(join(f.root, "deleted.md"), f.resolver),
    join(f.physical, "deleted.md"),
  );
});
