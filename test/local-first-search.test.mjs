import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { performance } from "node:perf_hooks";
import test from "node:test";
import { DaemonBackend } from "../dist/daemon/backend.js";
import { createZvecGrep } from "../dist/index.js";
import { FakeEmbeddingModel } from "./helpers/fake-embedding.mjs";

const noopWatchManagerFactory = () => ({
  start() {},
  flushPending: async () => {},
  close: async () => {},
});

test("indexed FTS works without constructing an unavailable embedding model", async () => {
  const fixture = await createIndexedFixture();
  let modelLoads = 0;
  const backend = new DaemonBackend({
    version: "1.0.0",
    watchManagerFactory: noopWatchManagerFactory,
    modelPoolOptions: {
      createModel: () => {
        modelLoads += 1;
        throw new Error("fixture embedding model is unavailable");
      },
    },
  });
  try {
    const result = await backend.search(ftsInput(fixture.root));
    assertContains(result, "pool-before-refresh");
    assert.equal(modelLoads, 0, "FTS must not acquire an embedding model");
    assert.deepEqual(backend.modelPool.snapshot(), {
      loaded: 0,
      activeLeases: 0,
    });
  } finally {
    await backend.close();
    await fixture.close();
  }
});

test("a held ordinary hybrid embedding does not block another indexed FTS request", async () => {
  const fixture = await createIndexedFixture();
  const entered = deferred();
  const released = deferred();
  let holdQuery = false;
  let queryEmbeddings = 0;
  class HeldQueryModel extends FakeEmbeddingModel {
    async doEmbed(contents, options) {
      if (options.purpose === "query") {
        queryEmbeddings += 1;
        if (holdQuery) {
          entered.resolve();
          await released.promise;
        }
      }
      return super.doEmbed(contents);
    }
  }
  const backend = new DaemonBackend({
    version: "1.0.0",
    watchManagerFactory: noopWatchManagerFactory,
    modelPoolOptions: { createModel: () => new HeldQueryModel() },
  });
  const pending = [];
  let hybridSettled = false;
  try {
    // Pay for the first native collection open before testing queue isolation.
    assertContains(
      await backend.search(ftsInput(fixture.root)),
      "pool-before-refresh",
    );
    assert.equal(queryEmbeddings, 0);
    holdQuery = true;
    const hybrid = observe(
      backend.search(hybridInput(fixture.root)).finally(() => {
        hybridSettled = true;
      }),
      pending,
    );
    await deadline(entered.promise, "hybrid query did not reach embedding");
    const local = observe(backend.search(ftsInput(fixture.root)), pending);
    const result = await deadline(
      local,
      "indexed FTS queued behind the held hybrid embedding",
    );
    assertContains(result, "pool-before-refresh");
    assert.equal(hybridSettled, false);
    assert.equal(queryEmbeddings, 1);
    released.resolve();
    assertContains(await hybrid, "pool-before-refresh");
  } finally {
    released.resolve();
    await Promise.allSettled(pending);
    await backend.close();
    await fixture.close();
  }
});

test("indexing advances during held hybrid embedding and search reads the refreshed generation", async () => {
  const fixture = await createIndexedFixture();
  const entered = deferred();
  const released = deferred();
  const documentEntered = deferred();
  let queryEmbeddings = 0;
  let documentEmbeddings = 0;
  let heldAt;
  class HeldQueryModel extends FakeEmbeddingModel {
    async doEmbed(contents, options) {
      if (options.purpose === "query") {
        queryEmbeddings += 1;
        heldAt = performance.now();
        entered.resolve();
        await released.promise;
      } else {
        documentEmbeddings += 1;
        documentEntered.resolve();
      }
      return super.doEmbed(contents);
    }
  }
  const backend = new DaemonBackend({
    version: "1.0.0",
    watchManagerFactory: noopWatchManagerFactory,
    modelPoolOptions: { createModel: () => new HeldQueryModel() },
  });
  const pending = [];
  let hybridSettled = false;
  try {
    assertContains(
      await backend.search(ftsInput(fixture.root)),
      "pool-before-refresh",
    );
    const hybridStartedAt = performance.now();
    const hybrid = observe(
      backend.search(hybridInput(fixture.root)).finally(() => {
        hybridSettled = true;
      }),
      pending,
    );
    await deadline(entered.promise, "hybrid query did not reach embedding");
    await writeFile(
      fixture.source,
      'export function connectionPool() { return "pool-after-refresh-current"; }\n',
    );
    const indexing = observe(
      backend.index({ root: fixture.root, wait: true }),
      pending,
    );
    await deadline(
      documentEntered.promise,
      "index writer queued behind the held hybrid embedding",
      5_000,
    );
    assert.equal(hybridSettled, false);
    // The gate above proves actual writer progress independently of native
    // collection commit/close time, which varies with concurrent CPU load.
    const indexed = await deadline(
      indexing,
      "index writer did not complete while hybrid embedding was held",
      15_000,
    );
    assert.equal(indexed.state, "succeeded");
    assert.ok(documentEmbeddings > 0, "writer must index the changed source");
    assert.equal(hybridSettled, false);
    assert.equal(queryEmbeddings, 1);
    assertContains(
      await deadline(
        observe(backend.search(ftsInput(fixture.root)), pending),
        "refreshed FTS did not finish while hybrid embedding was held",
      ),
      "pool-after-refresh-current",
    );
    const heldDurationMs = performance.now() - heldAt;
    released.resolve();
    const result = await hybrid;
    const hybridDurationMs = performance.now() - hybridStartedAt;
    assertContains(result, "pool-after-refresh-current");
    assert.ok(
      result.result.items.every(
        (item) => !item.content.includes("pool-before-refresh"),
      ),
      "prepared vectors must not resume against the retired read generation",
    );
    const timings = new Map(
      result.result.diagnostics.timings.map((entry) => [
        entry.name,
        entry.durationMs,
      ]),
    );
    for (const name of ["total", "query_embedding", "storage_total"]) {
      assert.ok(Number.isFinite(timings.get(name)), `${name} must be timed`);
    }
    assert.ok(timings.get("query_embedding") >= heldDurationMs - 1);
    assert.ok(timings.get("total") >= timings.get("query_embedding") - 1);
    assert.ok(timings.get("total") <= hybridDurationMs + 1);
    assert.ok(timings.get("storage_total") <= timings.get("total") + 1);
  } finally {
    released.resolve();
    await Promise.allSettled(pending);
    await backend.close();
    await fixture.close();
  }
});

async function createIndexedFixture() {
  const directory = await mkdtemp(join(tmpdir(), "zvec-local-first-search-"));
  const root = join(directory, "repo");
  const source = join(root, "pool.ts");
  const close = () => rm(directory, { recursive: true, force: true });
  try {
    await mkdir(root);
    await writeFile(
      source,
      'export function connectionPool() { return "pool-before-refresh"; }\n',
    );
    const service = await createZvecGrep({
      root,
      embeddingModel: new FakeEmbeddingModel(),
    });
    try {
      await service.index();
    } finally {
      await service.close();
    }
    return { root, source, close };
  } catch (error) {
    await close();
    throw error;
  }
}

function ftsInput(root) {
  return {
    root,
    queries: [],
    routes: [{ mode: "fts", query: "connectionPool" }],
    freshness: "eventual",
    autoUpdate: false,
  };
}

function hybridInput(root) {
  return {
    root,
    queries: ["how does the pool reuse active connections"],
    routes: [],
    freshness: "eventual",
    autoUpdate: false,
  };
}

function assertContains(response, text) {
  assert.ok(
    response.result.items.some((item) => item.content.includes(text)),
    `expected indexed source containing ${JSON.stringify(text)}`,
  );
}

function deferred() {
  let resolve;
  const promise = new Promise((fulfill) => {
    resolve = fulfill;
  });
  return { promise, resolve };
}

function observe(promise, pending) {
  pending.push(promise);
  void promise.catch(() => {});
  return promise;
}

async function deadline(promise, message, timeoutMs = 2_000) {
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
