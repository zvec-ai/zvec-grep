import assert from "node:assert/strict";
import {
  mkdir,
  mkdtemp,
  realpath,
  rm,
  stat,
  unlink,
  utimes,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { DaemonBackend } from "../dist/daemon/backend.js";
import { EngineError } from "../dist/engine/errors.js";
import { createZvecGrep } from "../dist/index.js";
import { FakeEmbeddingModel } from "./helpers/fake-embedding.mjs";

const originalSource =
  'export function connectionPool() { return "source-version-one"; }\n';
const currentSource =
  'export function connectionPool() { return "source-version-two"; }\n';
const subsequentSource =
  'export function connectionPool() { return "source-version-new"; }\n';
const noopWatchManagerFactory = () => ({
  start() {},
  flushPending: async () => {},
  close: async () => {},
});

class FakeLocalModel extends FakeEmbeddingModel {
  info = {
    ...this.info,
    reference: "local/potion-code-16m-v2",
    provider: "local",
    name: "potion-code-16m-v2",
  };
}

test("eventual ordinary search marks hash-detected drift stale and repairs it in the background", async () => {
  const rig = await createRig({ holdRepair: true });
  try {
    await rig.warm();
    await rig.changeWithoutStatDrift();
    const stale = await deadline(
      rig.observe(
        rig.backend.search(
          {
            root: rig.root,
            queries: ["connectionPool"],
            routes: [],
            freshness: "eventual",
            autoUpdate: true,
          },
          { signal: rig.signal },
        ),
      ),
      "eventual search waited for its background repair",
    );
    assertOldStaleEvidence(stale);
    assert.equal(stale.freshness, "possibly_stale");
    await deadline(
      rig.repairEntered.promise,
      "search did not start repairing hash-detected drift",
    );
    assert.ok(rig.documentCalls > 0);
    assert.equal(rig.backend.scheduler.hasActiveRoot(rig.root), true);
    rig.repairReleased.resolve();
    await deadline(
      rig.backend.scheduler.waitForRootIdle(rig.root),
      "background repair did not finish",
      15_000,
    );
    assert.equal(rig.backend.scheduler.getByRoot(rig.root)?.state, "succeeded");
    const repaired = await rig.backend.search(ftsInput(rig.root));
    assertCurrentEvidence(repaired);
    assert.equal(repaired.freshness, "fresh");
  } finally {
    await rig.close();
  }
});

test("refresh off records hash-detected drift without scheduling an index job", async () => {
  const rig = await createRig();
  try {
    await rig.warm();
    await rig.changeWithoutStatDrift();
    const stale = await rig.backend.search(ftsInput(rig.root));
    assertOldStaleEvidence(stale);
    assert.equal(stale.freshness, "possibly_stale");
    const repeated = await rig.backend.search(ftsInput(rig.root));
    assertOldStaleEvidence(repeated);
    assert.equal(repeated.freshness, "possibly_stale");
    assert.equal(rig.backend.scheduler.getByRoot(rig.root), undefined);
    assert.deepEqual(rig.backend.scheduler.snapshot(), {
      queued: 0,
      running: 0,
    });
    assert.equal(rig.documentCalls, 0);
    assert.equal(
      rig.modelLoads,
      0,
      "FTS with refresh off must not load a model to repair",
    );
  } finally {
    await rig.close();
  }
});

test("wait_for_fresh repairs hash-detected drift within the same search request", async () => {
  const rig = await createRig({ holdRepair: true });
  try {
    await rig.warm();
    await rig.changeWithoutStatDrift();
    const searching = rig.observe(
      rig.backend.search(
        ftsInput(rig.root, {
          freshness: "wait_for_fresh",
          autoUpdate: true,
        }),
        { signal: rig.signal },
      ),
    );
    const firstEvent = await deadline(
      Promise.race([
        rig.repairEntered.promise.then(() => "repair_started"),
        searching.then(() => "search_returned"),
      ]),
      "freshness wait neither repaired nor returned",
    );
    assert.equal(
      firstEvent,
      "repair_started",
      "freshness wait returned the stale snapshot instead of repairing it",
    );
    rig.repairReleased.resolve();
    const repaired = await deadline(
      searching,
      "same-request repair did not complete",
      15_000,
    );
    assertCurrentEvidence(repaired);
    assert.equal(repaired.freshness, "fresh");
    await deadline(
      rig.backend.scheduler.waitForRootIdle(rig.root),
      "repair scheduler did not become idle",
      15_000,
    );
    assert.equal(rig.backend.scheduler.getByRoot(rig.root)?.state, "succeeded");
    assert.ok(rig.documentCalls > 0);
  } finally {
    await rig.close();
  }
});

test("wait_for_fresh repairs a second edit discovered while the first repair embeds", async () => {
  const firstEntered = deferred();
  const firstReleased = deferred();
  const embeddedSources = [];
  const rig = await createRig({
    beforeDocument: async (contents, options) => {
      embeddedSources.push(contents.map((content) => content.text).join("\n"));
      if (embeddedSources.length === 1) {
        firstEntered.resolve();
        await waitForRelease(firstReleased.promise, options.signal);
      }
    },
  });
  try {
    await rig.warm();
    await rig.changeWithoutStatDrift(currentSource);
    const searching = rig.search(
      ftsInput(rig.root, { freshness: "wait_for_fresh", autoUpdate: true }),
    );
    assert.equal(
      await deadline(
        Promise.race([
          firstEntered.promise.then(() => "repair_started"),
          searching.then(() => "search_returned"),
        ]),
        "freshness wait did not reach the first held repair",
      ),
      "repair_started",
      "freshness wait returned before repairing the first edit",
    );
    assert.equal(rig.documentCalls, 1);
    assert.match(embeddedSources[0], /source-version-two/);
    const firstJob = rig.backend.scheduler.getByRoot(rig.root);
    assert.equal(firstJob?.state, "running");

    // The first repair has already read B. Keeping C's size and mtime equal
    // means only its post-write content proof can discover this newer edit.
    await rig.changeWithoutStatDrift(subsequentSource);
    firstReleased.resolve();
    const repaired = await deadline(
      searching,
      "freshness wait did not complete both bounded repair passes",
      30_000,
    );
    assert.equal(repaired.freshness, "fresh");
    assert.ok(
      repaired.result.items.some((item) =>
        item.content.includes("source-version-new"),
      ),
    );
    assert.ok(
      repaired.result.items.every(
        (item) =>
          item.status === "fresh" &&
          !/source-version-(one|two)/.test(item.content),
      ),
    );
    assert.equal(rig.documentCalls, 2, "the same request repairs B then C");
    assert.match(embeddedSources[1], /source-version-new/);
    await deadline(
      rig.backend.scheduler.waitForRootIdle(rig.root),
      "second repair did not leave the scheduler idle",
      15_000,
    );
    const finalJob = rig.backend.scheduler.getByRoot(rig.root);
    assert.equal(finalJob?.state, "succeeded");
    assert.notEqual(finalJob?.id, firstJob.id);
    assert.equal(rig.backend.scheduler.hasActiveRoot(rig.root), false);
  } finally {
    firstReleased.resolve();
    await rig.close();
  }
});

test("repeated ordinary searches coalesce behind one held hash-drift repair", async () => {
  const rig = await createRig({ holdRepair: true });
  try {
    await rig.warm();
    await rig.changeWithoutStatDrift();
    const initial = await deadline(
      rig.search(ordinaryInput(rig.root)),
      "first search did not return",
    );
    assert.equal(initial.freshness, "possibly_stale");
    await deadline(
      rig.repairEntered.promise,
      "hash-drift repair did not enter embedding",
    );
    const active = rig.backend.scheduler.getByRoot(rig.root);
    assert.ok(active);
    assert.equal(active.state, "running");
    const repeated = await deadline(
      Promise.all(
        Array.from({ length: 4 }, () => rig.search(ordinaryInput(rig.root))),
      ),
      "repeated searches waited on the held repair",
    );
    for (const response of repeated)
      assert.equal(response.freshness, "possibly_stale");
    assert.equal(
      rig.backend.scheduler.getByRoot(rig.root)?.id,
      active.id,
      "same observed content must not enqueue a repair per query",
    );
    assert.equal(rig.documentCalls, 1);
    rig.repairReleased.resolve();
    await deadline(
      rig.backend.scheduler.waitForRootIdle(rig.root),
      "coalesced repair did not finish",
      15_000,
    );
    const finished = rig.backend.scheduler.getByRoot(rig.root);
    assert.equal(
      finished?.id,
      active.id,
      "duplicate evidence must not leave a follow-up job",
    );
    assert.equal(finished?.state, "succeeded");
    assert.equal(rig.documentCalls, 1);
    assertCurrentEvidence(await rig.backend.search(ftsInput(rig.root)));
  } finally {
    await rig.close();
  }
});

test("failed hash-drift repair stays terminal for the same content but a later edit can repair", async () => {
  let failRepair = true;
  const rig = await createRig({
    beforeDocument() {
      if (failRepair) {
        throw new EngineError("fixture local model load failure", {
          code: "ZVEC_GREP.ENGINE.MODELS.MODEL2VEC_LOAD_FAILED",
        });
      }
    },
    schedulerOptions: { maxAttempts: 1 },
  });
  try {
    await rig.warm();
    await rig.changeWithoutStatDrift();
    const stale = await deadline(
      rig.search(ordinaryInput(rig.root)),
      "initial search did not return",
    );
    assert.equal(stale.freshness, "possibly_stale");
    await deadline(
      rig.repairEntered.promise,
      "repair never attempted document embedding",
    );
    await deadline(
      rig.backend.scheduler.waitForRootIdle(rig.root),
      "failed repair did not become terminal",
      15_000,
    );
    const failed = rig.backend.scheduler.getByRoot(rig.root);
    assert.equal(failed?.state, "failed");
    const failedCalls = rig.documentCalls;
    assert.equal(failedCalls, 1);
    for (let repeat = 0; repeat < 3; repeat++) {
      const response = await deadline(
        rig.search(ordinaryInput(rig.root)),
        "same-content query did not return after failure",
      );
      assert.equal(response.freshness, "possibly_stale");
      assert.equal(rig.backend.scheduler.getByRoot(rig.root)?.id, failed.id);
      assert.equal(rig.backend.scheduler.hasActiveRoot(rig.root), false);
    }
    assert.equal(
      rig.documentCalls,
      failedCalls,
      "queries must not repeatedly re-embed an unchanged failed revision",
    );

    failRepair = false;
    await rig.changeWithoutStatDrift(subsequentSource);
    const next = await deadline(
      rig.search(ordinaryInput(rig.root)),
      "changed-content search did not return",
    );
    assert.equal(next.freshness, "possibly_stale");
    await deadline(
      rig.backend.scheduler.waitForRootIdle(rig.root),
      "later content could not repair after an earlier failure",
      15_000,
    );
    const retried = rig.backend.scheduler.getByRoot(rig.root);
    assert.notEqual(
      retried?.id,
      failed.id,
      "a new content revision must not inherit permanent suppression",
    );
    assert.equal(retried?.state, "succeeded");
    assert.equal(rig.documentCalls, failedCalls + 1);
    const repaired = await rig.backend.search(ftsInput(rig.root));
    assert.equal(repaired.freshness, "fresh");
    assert.ok(
      repaired.result.items.some((item) =>
        item.content.includes("source-version-new"),
      ),
    );
    assert.ok(
      repaired.result.items.every(
        (item) =>
          item.status === "fresh" &&
          !item.content.includes("source-version-one"),
      ),
    );
  } finally {
    await rig.close();
  }
});

test("cancelled hash-drift repair is not resubmitted by every same-content query", async () => {
  const rig = await createRig({ holdRepair: true });
  try {
    await rig.warm();
    await rig.changeWithoutStatDrift();
    const stale = await deadline(
      rig.search(ordinaryInput(rig.root)),
      "initial search did not return",
    );
    assert.equal(stale.freshness, "possibly_stale");
    await deadline(
      rig.repairEntered.promise,
      "repair did not reach its held embedding",
    );
    const active = rig.backend.scheduler.getByRoot(rig.root);
    assert.ok(active);
    assert.equal(rig.backend.scheduler.cancelRoot(rig.root), true);
    await deadline(
      rig.backend.scheduler.waitForRootIdle(rig.root),
      "cancelled repair did not release its job",
      15_000,
    );
    assert.equal(rig.backend.scheduler.getByRoot(rig.root)?.state, "cancelled");
    const calls = rig.documentCalls;
    for (let repeat = 0; repeat < 3; repeat++) {
      const response = await deadline(
        rig.search(ordinaryInput(rig.root)),
        "same-content search hung after cancellation",
      );
      assert.equal(response.freshness, "possibly_stale");
      assert.equal(rig.backend.scheduler.getByRoot(rig.root)?.id, active.id);
      assert.equal(rig.backend.scheduler.hasActiveRoot(rig.root), false);
    }
    assert.equal(rig.documentCalls, calls);
  } finally {
    await rig.close();
  }
});

test("missing source evidence schedules deletion repair and removes old search results", async () => {
  const rig = await createRig();
  try {
    await rig.warm();
    await rig.deleteSource();
    const stale = await deadline(
      rig.search(ftsInput(rig.root, { autoUpdate: true })),
      "missing-source search did not return",
    );
    assertOldStaleEvidence(stale);
    assert.equal(stale.freshness, "possibly_stale");
    const job = rig.backend.scheduler.getByRoot(rig.root);
    assert.ok(job, "missing source evidence must schedule index cleanup");
    await deadline(
      rig.backend.scheduler.waitForRootIdle(rig.root),
      "deletion repair did not finish",
      15_000,
    );
    assert.equal(rig.backend.scheduler.getByRoot(rig.root)?.state, "succeeded");
    const repaired = await rig.backend.search(ftsInput(rig.root));
    assert.equal(repaired.freshness, "fresh");
    assert.deepEqual(repaired.result.items, []);
    assert.equal(
      rig.documentCalls,
      0,
      "deleting stale documents does not need source embeddings",
    );
  } finally {
    await rig.close();
  }
});

async function createRig({
  holdRepair = false,
  beforeDocument = () => {},
  schedulerOptions,
} = {}) {
  const directory = await realpath(
    await mkdtemp(join(tmpdir(), "zvec-search-healing-")),
  );
  const root = join(directory, "repo");
  const source = join(root, "pool.ts");
  const repairEntered = deferred();
  const repairReleased = deferred();
  const searchController = new AbortController();
  const pending = [];
  let backend;
  let documentCalls = 0;
  let modelLoads = 0;
  try {
    await mkdir(root);
    await writeFile(source, originalSource);
    // Integral timestamps avoid filesystem precision differences obscuring the
    // intended preserved-size-and-mtime case on macOS or Linux.
    await utimes(source, 1_600_000_000, 1_600_000_000);
    const originalStat = await stat(source);
    const service = await createZvecGrep({
      root,
      embeddingModel: new FakeLocalModel(),
    });
    try {
      await service.index();
    } finally {
      await service.close();
    }
    class RepairModel extends FakeLocalModel {
      async doEmbed(contents, options) {
        if (options.purpose === "document") {
          documentCalls += 1;
          repairEntered.resolve();
          if (holdRepair)
            await waitForRelease(repairReleased.promise, options.signal);
          await beforeDocument(contents, options);
        }
        return super.doEmbed(contents, options);
      }
    }
    backend = new DaemonBackend({
      version: "test",
      watchManagerFactory: noopWatchManagerFactory,
      schedulerOptions,
      modelPoolOptions: {
        createModel: () => {
          modelLoads += 1;
          return new RepairModel();
        },
      },
    });
    return {
      root,
      backend,
      signal: searchController.signal,
      repairEntered,
      repairReleased,
      get documentCalls() {
        return documentCalls;
      },
      get modelLoads() {
        return modelLoads;
      },
      observe(promise) {
        pending.push(promise);
        void promise.catch(() => {});
        return promise;
      },
      search(input) {
        const promise = backend.search(input, {
          signal: searchController.signal,
        });
        pending.push(promise);
        void promise.catch(() => {});
        return promise;
      },
      async warm() {
        const initial = await backend.search(ftsInput(root));
        assert.equal(initial.freshness, "fresh");
        assert.ok(
          initial.result.items.some((item) =>
            item.content.includes("source-version-one"),
          ),
        );
        assert.ok(
          initial.result.items.every((item) => item.status === "fresh"),
        );
        assert.equal(backend.scheduler.getByRoot(root), undefined);
      },
      async changeWithoutStatDrift(nextSource = currentSource) {
        assert.equal(
          Buffer.byteLength(nextSource),
          Buffer.byteLength(originalSource),
        );
        await writeFile(source, nextSource);
        await utimes(source, originalStat.atime, originalStat.mtime);
        const current = await stat(source);
        assert.equal(current.size, originalStat.size);
        assert.equal(current.mtimeMs, originalStat.mtimeMs);
      },
      deleteSource() {
        return unlink(source);
      },
      async close() {
        repairReleased.resolve();
        searchController.abort(
          new Error("fixture cleanup cancelled pending search"),
        );
        await Promise.allSettled(pending);
        try {
          await backend.close();
        } finally {
          await rm(directory, { recursive: true, force: true });
        }
      },
    };
  } catch (error) {
    repairReleased.resolve();
    await backend?.close();
    await rm(directory, { recursive: true, force: true });
    throw error;
  }
}

function ftsInput(root, overrides = {}) {
  return {
    root,
    queries: [],
    routes: [{ mode: "fts", query: "connectionPool" }],
    freshness: "eventual",
    autoUpdate: false,
    ...overrides,
  };
}

function ordinaryInput(root) {
  return {
    root,
    queries: ["connectionPool"],
    routes: [],
    freshness: "eventual",
    autoUpdate: true,
  };
}

async function waitForRelease(release, signal) {
  signal?.throwIfAborted();
  let onAbort;
  try {
    await Promise.race([
      release,
      new Promise((_, reject) => {
        onAbort = () => reject(signal.reason);
        signal?.addEventListener("abort", onAbort, { once: true });
      }),
    ]);
  } finally {
    signal?.removeEventListener("abort", onAbort);
  }
}

function assertOldStaleEvidence(response) {
  assert.ok(
    response.result.items.some((item) =>
      item.content.includes("source-version-one"),
    ),
  );
  assert.ok(
    response.result.items.every((item) => item.status === "possibly_stale"),
  );
}

function assertCurrentEvidence(response) {
  assert.ok(
    response.result.items.some((item) =>
      item.content.includes("source-version-two"),
    ),
  );
  assert.ok(response.result.items.every((item) => item.status === "fresh"));
  assert.ok(
    response.result.items.every(
      (item) => !item.content.includes("source-version-one"),
    ),
  );
}

function deferred() {
  let resolve;
  const promise = new Promise((fulfill) => {
    resolve = fulfill;
  });
  return { promise, resolve };
}

async function deadline(promise, message, timeoutMs = 10_000) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(message)), timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}
