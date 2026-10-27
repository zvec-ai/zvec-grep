import assert from "node:assert/strict";
import fs from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import {
  mkdir,
  mkdtemp,
  realpath,
  rename,
  rm,
  symlink,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { WorkspaceBindingStore } from "../../dist/engine/bindings.js";

test("F1 binding uses native paths across short-name aliases and invalidation", async (t) => {
  const base = await mkdtemp(join(tmpdir(), "zg-binding-native-"));
  t.after(() => rm(base, { recursive: true, force: true }));
  const root = join(base, "long-workspace-name");
  const alias = join(base, "SHORT~1");
  await mkdir(join(root, ".zvec-grep"), { recursive: true });
  await symlink(root, alias, process.platform === "win32" ? "junction" : "dir");
  const canonical = await realpath(root);
  const original = fs.realpathSync;
  // Model Windows' JS realpath behavior for an 8.3 alias. Native realpath
  // and stat still use the real filesystem and the same physical directory.
  const jsRealpath = (...args) =>
    args[0] === alias ? alias : original(...args);
  jsRealpath.native = original.native;
  t.mock.method(fs, "realpathSync", jsRealpath);
  syncBuiltinESMExports();
  t.after(() => {
    t.mock.restoreAll();
    syncBuiltinESMExports();
  });
  const store = new WorkspaceBindingStore(join(base, "home"));
  store.record("fixture", alias);
  assert.equal(
    store.matches("fixture", canonical),
    true,
    "a verified alias must match the daemon's native path",
  );
  store.invalidate("fixture", canonical);
  assert.equal(
    store.matches("fixture", alias),
    false,
    "invalidation through the native path must remove alias verification",
  );
  store.record("fixture", canonical);
  store.invalidate("fixture", alias);
  assert.equal(
    store.matches("fixture", canonical),
    false,
    "invalidation through the alias must remove native verification",
  );
  store.record("fixture", alias);
  await rename(join(root, ".zvec-grep"), join(root, "old-index"));
  await mkdir(join(root, ".zvec-grep"));
  assert.equal(
    store.matches("fixture", canonical),
    false,
    "a replacement storage directory must remain unverified",
  );
});

test("F1 real temporary paths retain binding through async native resolution", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "zg-binding-temp-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await mkdir(join(root, ".zvec-grep"));
  const canonical = await realpath(root);
  t.diagnostic(
    JSON.stringify({ root, js: fs.realpathSync(root), native: canonical }),
  );
  const store = new WorkspaceBindingStore(join(root, "home"));
  store.record("fixture", root);
  assert.equal(store.matches("fixture", canonical), true);
});
