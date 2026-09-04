import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { WatchManager } from "../dist/daemon/watch-manager.js";

test("watch manager debounces file changes and reports overflow reconciliation", async () => {
  const temporaryDirectory = await mkdtemp(join(tmpdir(), "zvec-grep-watch-"));
  const root = join(temporaryDirectory, "repo");
  await mkdir(join(root, "src"), { recursive: true });
  await writeFile(join(root, "src", "a.ts"), "export const a = 1;\n");
  let listener;
  const watcher = new EventEmitter();
  watcher.close = () => {};
  const batches = [];
  const manager = new WatchManager({
    root,
    platform: "darwin",
    debounceMs: 5,
    maxWaitMs: 20,
    reconcileIntervalMs: 0,
    watchFactory: (_root, _options, callback) => {
      listener = callback;
      return watcher;
    },
    onChanges: (changes) => {
      batches.push(changes);
    },
  });
  try {
    manager.start();
    listener("change", "src/a.ts");
    listener("change", "src/a.ts");
    await waitFor(() => batches.length === 1);
    assert.deepEqual(batches[0].touchedFiles, [join(root, "src", "a.ts")]);
    watcher.emit("error", new Error("overflow"));
    await waitFor(() => batches.length === 2);
    assert.equal(batches[1].forceFullReconcile, true);
  } finally {
    await manager.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("watch manager uses per-directory watchers on Linux", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watch-fallback-"),
  );
  const root = join(temporaryDirectory, "repo");
  const nested = join(root, "src");
  await mkdir(nested, { recursive: true });
  await writeFile(join(nested, "a.ts"), "export const a = 1;\n");
  const listeners = new Map();
  const batches = [];
  let recursiveAttempts = 0;
  const manager = new WatchManager({
    root,
    platform: "linux",
    debounceMs: 5,
    maxWaitMs: 20,
    reconcileIntervalMs: 0,
    watchFactory: (directory, options, callback) => {
      if (options.recursive) {
        recursiveAttempts += 1;
        throw new Error("recursive unsupported");
      }
      const fallback = new EventEmitter();
      fallback.close = () => {};
      listeners.set(directory, callback);
      return fallback;
    },
    onChanges: (changes) => {
      batches.push(changes);
    },
  });
  try {
    manager.start();
    await waitFor(() => listeners.has(nested));
    listeners.get(nested)("change", "a.ts");
    await waitFor(() => batches.length === 1);
    assert.deepEqual(batches[0].touchedFiles, [join(nested, "a.ts")]);
    assert.equal(recursiveAttempts, 0);
  } finally {
    await manager.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("watch manager drops ignored file events before creating a change batch", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watch-ignore-"),
  );
  const root = join(temporaryDirectory, "repo");
  const dependency = join(root, "node_modules", "pkg", "index.js");
  await mkdir(join(root, "node_modules", "pkg"), { recursive: true });
  await writeFile(join(root, ".gitignore"), "node_modules/\n");
  await writeFile(dependency, "module.exports = true;\n");
  let listener;
  const watcher = new EventEmitter();
  watcher.close = () => {};
  const batches = [];
  const pending = [];
  const manager = new WatchManager({
    root,
    platform: "darwin",
    debounceMs: 5,
    maxWaitMs: 20,
    reconcileIntervalMs: 0,
    getRootPaths: () => [{ absolutePath: root, recursive: true }],
    watchFactory: (_root, _options, callback) => {
      listener = callback;
      return watcher;
    },
    onPendingChange: (value) => pending.push(value),
    onChanges: (changes) => batches.push(changes),
  });
  try {
    manager.start();
    listener("change", "node_modules/pkg/index.js");
    await new Promise((resolve) => setTimeout(resolve, 30));
    assert.equal(batches.length, 0);
    assert.deepEqual(pending, []);

    listener("change", ".gitignore");
    await waitFor(() => batches.length === 1);
    assert.deepEqual(batches[0].rescanDirectories, [root]);
  } finally {
    await manager.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("fallback watcher prunes ignored directories and restores newly included ones", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watch-ignore-fallback-"),
  );
  const root = join(temporaryDirectory, "repo");
  const source = join(root, "src");
  const dependencies = join(root, "node_modules");
  await mkdir(source, { recursive: true });
  await mkdir(join(dependencies, "pkg"), { recursive: true });
  await writeFile(join(root, ".gitignore"), "node_modules/\n");
  const listeners = new Map();
  const manager = new WatchManager({
    root,
    platform: "linux",
    debounceMs: 5,
    maxWaitMs: 20,
    reconcileIntervalMs: 0,
    getRootPaths: () => [{ absolutePath: root, recursive: true }],
    watchFactory: (directory, options, callback) => {
      assert.equal(options.recursive, false);
      const watcher = new EventEmitter();
      watcher.close = () => {};
      listeners.set(directory, callback);
      return watcher;
    },
    onChanges: () => {},
  });
  try {
    manager.start();
    await waitFor(() => listeners.has(source));
    assert.equal(listeners.has(dependencies), false);

    await writeFile(join(root, ".gitignore"), "!node_modules/\n");
    listeners.get(root)("change", ".gitignore");
    await waitFor(() => listeners.has(dependencies));
  } finally {
    await manager.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("fallback watcher honors noIgnore when selecting directories", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watch-no-ignore-"),
  );
  const root = join(temporaryDirectory, "repo");
  const dependencies = join(root, "node_modules");
  await mkdir(dependencies, { recursive: true });
  await writeFile(join(root, ".gitignore"), "node_modules/\n");
  const watched = new Set();
  const manager = new WatchManager({
    root,
    platform: "linux",
    reconcileIntervalMs: 0,
    getRootPaths: () => [
      { absolutePath: root, recursive: true, noIgnore: true },
    ],
    watchFactory: (directory, options) => {
      assert.equal(options.recursive, false);
      const watcher = new EventEmitter();
      watcher.close = () => {};
      watched.add(directory);
      return watcher;
    },
    onChanges: () => {},
  });
  try {
    manager.start();
    await waitFor(() => watched.has(dependencies));
  } finally {
    await manager.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("fallback watcher mirrors scanner hidden-directory selection", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watch-hidden-"),
  );
  const root = join(temporaryDirectory, "repo");
  const source = join(root, "src");
  const idea = join(root, ".idea");
  const vscode = join(root, ".vscode");
  const git = join(root, ".git");
  const metadata = join(root, ".zvec-grep");
  await Promise.all(
    [source, idea, vscode, git, metadata].map((directory) =>
      mkdir(directory, { recursive: true }),
    ),
  );

  const run = async (rootPath) => {
    const watched = new Set();
    const manager = new WatchManager({
      root,
      platform: "linux",
      reconcileIntervalMs: 0,
      getRootPaths: () => [rootPath],
      watchFactory: (directory, options) => {
        assert.equal(options.recursive, false);
        const watcher = new EventEmitter();
        watcher.close = () => {};
        watched.add(directory);
        return watcher;
      },
      onChanges: () => {},
    });
    manager.start();
    await waitFor(() => watched.has(source));
    await new Promise((resolve) => setTimeout(resolve, 10));
    await manager.close();
    return watched;
  };

  try {
    const defaults = await run({ absolutePath: root, recursive: true });
    assert.equal(defaults.has(idea), false);
    assert.equal(defaults.has(vscode), false);

    const withHidden = await run({
      absolutePath: root,
      recursive: true,
      hidden: true,
    });
    assert.equal(withHidden.has(idea), true);
    assert.equal(withHidden.has(vscode), true);
    assert.equal(withHidden.has(git), false);
    assert.equal(withHidden.has(metadata), false);

    const withInclude = await run({
      absolutePath: root,
      recursive: true,
      include: [".vscode/settings.json"],
    });
    assert.equal(withInclude.has(idea), false);
    assert.equal(withInclude.has(vscode), true);
  } finally {
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("watch manager compacts an exact event storm into one directory scan", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watch-storm-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  for (let index = 0; index < 4; index++) {
    await writeFile(
      join(root, `${index}.ts`),
      `export const value${index} = ${index};\n`,
    );
  }
  let listener;
  const watcher = new EventEmitter();
  watcher.close = () => {};
  const batches = [];
  const manager = new WatchManager({
    root,
    platform: "darwin",
    debounceMs: 10,
    maxWaitMs: 30,
    reconcileIntervalMs: 0,
    maxChangedPaths: 3,
    watchFactory: (_root, _options, callback) => {
      listener = callback;
      return watcher;
    },
    onChanges: (changes) => {
      batches.push(changes);
    },
  });
  try {
    manager.start();
    for (let index = 0; index < 4; index++) listener("change", `${index}.ts`);
    await waitFor(() => batches.length === 1);
    assert.deepEqual(batches[0], {
      touchedFiles: [],
      rescanDirectories: [root],
      deletedPrefixes: [],
      forceFullReconcile: false,
    });
    assert.equal(batches.length, 1);
  } finally {
    await manager.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("fallback watcher reattaches after a directory is deleted and recreated", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watch-recreate-"),
  );
  const root = join(temporaryDirectory, "repo");
  const nested = join(root, "src");
  await mkdir(nested, { recursive: true });
  const listeners = new Map();
  const creations = new Map();
  const manager = new WatchManager({
    root,
    debounceMs: 5,
    maxWaitMs: 20,
    reconcileIntervalMs: 0,
    resumeCheckIntervalMs: 0,
    watchFactory: (directory, options, callback) => {
      if (options.recursive) throw new Error("recursive unsupported");
      const fallback = new EventEmitter();
      fallback.close = () => {};
      listeners.set(directory, callback);
      creations.set(directory, (creations.get(directory) ?? 0) + 1);
      return fallback;
    },
    onChanges: () => {},
  });
  try {
    manager.start();
    await waitFor(() => creations.get(nested) === 1);
    await rm(nested, { recursive: true, force: true });
    listeners.get(root)("rename", "src");
    await new Promise((resolve) => setTimeout(resolve, 10));
    await mkdir(nested);
    listeners.get(root)("rename", "src");
    await waitFor(() => creations.get(nested) === 2);
  } finally {
    await manager.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("resume drift requests reconciliation and pending state spans debounce", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watch-resume-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  await writeFile(join(root, "a.ts"), "export const a = 1;\n");
  let listener;
  const watcher = new EventEmitter();
  watcher.close = () => {};
  const pending = [];
  const reasons = [];
  const manager = new WatchManager({
    root,
    platform: "darwin",
    debounceMs: 20,
    maxWaitMs: 40,
    reconcileIntervalMs: 0,
    resumeCheckIntervalMs: 0,
    resumeThresholdMs: 100,
    watchFactory: (_root, _options, callback) => {
      listener = callback;
      return watcher;
    },
    onPendingChange: (value) => pending.push(value),
    onChanges: (_changes, reason) => reasons.push(reason),
  });
  try {
    manager.start();
    listener("change", "a.ts");
    await waitFor(() => pending.includes(true));
    assert.equal(pending.at(-1), true);
    await waitFor(() => reasons.length === 1);
    assert.equal(pending.at(-1), false);
    manager.checkForResume(Date.now() + 1_000);
    await waitFor(() => reasons.length === 2);
    assert.equal(reasons[1], "reconcile");
  } finally {
    await manager.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("watcher errors trigger reconciliation and replace the failed watcher", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watch-error-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const watchers = [];
  const reasons = [];
  const manager = new WatchManager({
    root,
    platform: "darwin",
    debounceMs: 5,
    maxWaitMs: 20,
    reconcileIntervalMs: 0,
    resumeCheckIntervalMs: 0,
    watchFactory: () => {
      const created = new EventEmitter();
      created.close = () => {};
      watchers.push(created);
      return created;
    },
    onChanges: (_changes, reason) => reasons.push(reason),
  });
  try {
    manager.start();
    watchers[0].emit("error", new Error("watch failed"));
    await waitFor(() => reasons.length === 1);
    await waitFor(() => watchers.length === 2);
    watchers[1].emit("error", new Error("watch still failed"));
    await waitFor(() => watchers.length === 3);
    assert.equal(reasons[0], "reconcile");
    assert.equal(reasons.length, 1);
  } finally {
    await manager.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("directory watcher retries are independent", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watch-multi-error-"),
  );
  const root = join(temporaryDirectory, "repo");
  const first = join(root, "first");
  const second = join(root, "second");
  await mkdir(first, { recursive: true });
  await mkdir(second);
  const watchers = new Map();
  const manager = new WatchManager({
    root,
    platform: "linux",
    debounceMs: 5,
    maxWaitMs: 20,
    reconcileIntervalMs: 0,
    resumeCheckIntervalMs: 0,
    watchFactory: (directory) => {
      const created = new EventEmitter();
      created.close = () => {};
      const existing = watchers.get(directory) ?? [];
      existing.push(created);
      watchers.set(directory, existing);
      return created;
    },
    onChanges: () => {},
  });
  try {
    manager.start();
    await waitFor(
      () =>
        watchers.get(first)?.length === 1 && watchers.get(second)?.length === 1,
    );
    watchers.get(first)[0].emit("error", new Error("first failed"));
    watchers.get(second)[0].emit("error", new Error("second failed"));
    await waitFor(
      () =>
        watchers.get(first)?.length === 2 && watchers.get(second)?.length === 2,
    );
  } finally {
    await manager.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  }
});

test("close waits for an in-flight async change callback", async () => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-watch-close-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  await writeFile(join(root, "a.ts"), "export const a = 1;\n");
  let listener;
  let release;
  let started = false;
  const watcher = new EventEmitter();
  watcher.close = () => {};
  const manager = new WatchManager({
    root,
    platform: "darwin",
    debounceMs: 5,
    maxWaitMs: 20,
    reconcileIntervalMs: 0,
    resumeCheckIntervalMs: 0,
    watchFactory: (_root, _options, callback) => {
      listener = callback;
      return watcher;
    },
    onChanges: () =>
      new Promise((resolve) => {
        started = true;
        release = resolve;
      }),
  });
  manager.start();
  listener("change", "a.ts");
  await waitFor(() => started);
  let closed = false;
  const closing = manager.close().then(() => {
    closed = true;
  });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(closed, false);
  release();
  await closing;
  assert.equal(closed, true);
  await rm(temporaryDirectory, { recursive: true, force: true });
});

async function waitFor(predicate) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error("Condition was not reached.");
}
