import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { performance } from "node:perf_hooks";
import test from "node:test";
import { ChangeSet } from "../dist/daemon/change-set.js";

test("change set folds child paths and invalidates gitignore subtrees", () => {
  const root = join(tmpdir(), "change-set-repo");
  const changes = new ChangeSet();
  changes.add(join(root, "src", "a.ts"), "changed");
  changes.add(join(root, "src", "b.ts"), "changed");
  changes.add(join(root, "src"), "created", true);
  changes.add(join(root, "packages", ".gitignore"), "changed");
  assert.deepEqual(changes.snapshot(), {
    touchedFiles: [],
    rescanDirectories: [join(root, "packages"), join(root, "src")].sort(),
    deletedPrefixes: [],
    forceFullReconcile: false,
  });
});

test("change set collapses deleted prefixes", () => {
  const root = join(tmpdir(), "change-set-storm-repo");
  const changes = new ChangeSet({ root, maxChangedPaths: 10 });
  changes.add(join(root, "src", "a.ts"), "deleted");
  changes.add(join(root, "src"), "deleted", true);
  changes.add(join(root, "other.ts"), "changed");
  changes.add(join(root, "third.ts"), "changed");
  const snapshot = changes.snapshot();
  assert.deepEqual(snapshot.deletedPrefixes, [join(root, "src")]);
  assert.equal(snapshot.forceFullReconcile, false);
});

test("change set batches large watcher bursts without blocking the event loop", () => {
  const root = join(tmpdir(), "change-set-large-burst-repo");
  const changes = new ChangeSet({ maxChangedPaths: 1_000 });
  const started = performance.now();
  for (let index = 0; index < 200; index += 1) {
    changes.add(join(root, `package-${index}`, "removed.ts"), "deleted");
  }
  const snapshot = changes.snapshot();
  const durationMs = performance.now() - started;

  assert.equal(snapshot.deletedPrefixes.length, 200);
  assert.ok(
    durationMs < 1_500,
    `processing 200 watcher paths took ${Math.round(durationMs)}ms`,
  );
});

test("change set compacts exact event storms without requiring full reconciliation", () => {
  const root = join(tmpdir(), "change-set-exact-overflow-repo");
  const changes = new ChangeSet({ root, maxChangedPaths: 3 });
  changes.add(join(root, "src", "a.ts"), "changed");
  changes.add(join(root, "src", "b.ts"), "changed");
  changes.add(join(root, "src", "c.ts"), "changed");

  assert.deepEqual(changes.snapshot(), {
    touchedFiles: [],
    rescanDirectories: [join(root, "src")],
    deletedPrefixes: [],
    forceFullReconcile: false,
  });
});

test("change set bounds twelve thousand exact paths to directory scopes", () => {
  const root = join(tmpdir(), "change-set-twelve-thousand-repo");
  const changes = new ChangeSet({ root, maxChangedPaths: 1_000 });
  const started = performance.now();
  for (let index = 0; index < 12_000; index += 1) {
    changes.add(
      join(root, `package-${Math.floor(index / 500)}`, `${index}.ts`),
      "changed",
    );
  }
  const snapshot = changes.snapshot();
  const durationMs = performance.now() - started;

  assert.equal(snapshot.forceFullReconcile, false);
  assert.equal(snapshot.touchedFiles.length, 0);
  assert.equal(snapshot.rescanDirectories.length, 24);
  assert.ok(
    durationMs < 1_500,
    `processing 12000 exact watcher paths took ${Math.round(durationMs)}ms`,
  );
});

test("change set retains a path after an explicit full reconciliation request", () => {
  const root = join(tmpdir(), "change-set-reconciliation-repo");
  const changes = new ChangeSet();
  changes.requireFullReconcile();
  changes.add(join(root, "changed.ts"), "changed");

  assert.deepEqual(changes.snapshot(), {
    touchedFiles: [join(root, "changed.ts")],
    rescanDirectories: [],
    deletedPrefixes: [],
    forceFullReconcile: true,
  });
});
