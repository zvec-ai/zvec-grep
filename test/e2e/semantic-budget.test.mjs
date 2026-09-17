import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { DaemonBackend } from "../../dist/daemon/backend.js";
import { DaemonHttpServer } from "../../dist/daemon/http-server.js";
import { DaemonInstanceLock } from "../../dist/daemon/server-controller.js";
import { createZvecGrep } from "../../dist/index.js";
import { FakeEmbeddingModel } from "../helpers/fake-embedding.mjs";
import {
  cliPath,
  createTemporaryDirectory,
  removeTemporaryDirectory,
} from "../helpers/fixtures.mjs";

const BUDGET_MS = 150;
const query = "connectionPool manages";
const currentSource =
  'export function connectionPool() { const manages = "CLI_POOL_CURRENT"; return manages; }\n';
const budgetWarning =
  "warning: semantic search exceeded the preparation budget; showing local text matches only (search coverage is incomplete)";

class FakeLocalModel extends FakeEmbeddingModel {
  info = {
    ...this.info,
    reference: "local/potion-code-16m-v2",
    provider: "local",
    name: "potion-code-16m-v2",
  };
}

test("a default CLI query returns useful local results while its own semantic gate remains held", async (t) => {
  const rig = await createRig(t);
  assert.equal(currentSource.includes(query), false);

  // No mode, --fts, --hybrid, refresh, or embedding flag: exercise ordinary
  // auto discovery, daemon admin/schema transport, live merge, and rendering.
  const searching = rig.run([query]);
  await deadline(rig.entered.promise, "default CLI bypassed the held model");
  const result = await deadline(
    searching.result,
    "default CLI waited for semantic work past its preparation budget",
  );
  assertSuccessful(result);
  assert.match(result.stdout, /pool\.ts/);
  assert.match(result.stdout, /CLI_POOL_CURRENT/);
  assert.match(result.stdout, /connectionPool/);
  assert.doesNotMatch(result.stdout, /matchedBy=keyword\b/);
  assertBudgetWarning(result.stderr);
  await deadline(rig.aborted.promise, "budget did not abort model preparation");
  assert.equal(rig.released, false, "the test never released semantic work");
  assert.equal(rig.queryCalls.length, 1);
  assert.equal(rig.queryCalls[0].signal?.aborted, true);
  assert.deepEqual(rig.responses[0].result.diagnostics.semantic, {
    status: "skipped",
    reason: "preparation_budget_exceeded",
    budgetMs: 1_000,
  });
  assert.deepEqual(
    rig.responses[0].result.groupResults.map(({ id, role, query }) => ({
      id,
      role,
      query,
    })),
    [{ id: "Q1", role: "primary", query }],
  );

  rig.holdQueries = false;
  const recovered = await deadline(
    rig.run([query]).result,
    "daemon could not serve the next ordinary CLI query",
  );
  assertSuccessful(recovered);
  assert.match(recovered.stdout, /CLI_POOL_CURRENT/);
  assert.doesNotMatch(recovered.stderr, /preparation budget|incomplete/);
  assert.equal(rig.queryCalls.length, 2);
  assert.equal(rig.released, false);
});

test("default CLI budget fallback rescans current keyword-only source when indexed FTS is empty", async (t) => {
  const rig = await createRig(t, {
    source: 'export function archivedLedger() { return "OLD_INDEX_ONLY"; }\n',
    budgetMs: BUDGET_MS,
  });
  const keywordQuery = "connection pool";
  const addedSource =
    'export function connectionPool() { return "CLI_CURRENT_KEYWORD"; }\n';
  assert.equal(addedSource.includes(keywordQuery), false);
  await writeFile(join(rig.root, "added.ts"), addedSource);

  // The new file is absent from the index and the full phrase is absent from
  // current source. Only the post-budget keyword rescan can recover this hit.
  const searching = rig.run([keywordQuery]);
  await deadline(rig.entered.promise, "keyword CLI bypassed the held model");
  const result = await deadline(
    searching.result,
    "empty indexed FTS did not yield current keyword evidence",
  );
  assertSuccessful(result);
  assert.match(result.stdout, /added\.ts/);
  assert.match(result.stdout, /CLI_CURRENT_KEYWORD/);
  assert.match(result.stdout, /matchedBy=keyword\b/);
  assert.doesNotMatch(result.stdout, /OLD_INDEX_ONLY|No .*matches/);
  assertBudgetWarning(result.stderr);
  await deadline(
    rig.aborted.promise,
    "keyword fallback did not abort semantics",
  );
  assert.equal(rig.released, false);
  assert.equal(rig.queryCalls.length, 1);
  assert.equal(rig.queryCalls[0].signal?.aborted, true);
});

test(
  "explicit --hybrid CLI waits beyond the default budget and remains cancellable",
  {
    skip:
      process.platform === "win32" && "child SIGINT is not portable on Windows",
  },
  async (t) => {
    const rig = await createRig(t, { budgetMs: BUDGET_MS });
    const searching = rig.run(["--hybrid", query]);
    await deadline(
      rig.entered.promise,
      "explicit hybrid did not reach the model",
    );
    await delay(BUDGET_MS * 2);
    assert.equal(searching.settled, false);
    assert.equal(rig.queryCalls[0].signal?.aborted, false);
    assert.equal(rig.released, false);

    assert.equal(searching.child.kill("SIGINT"), true);
    const cancelled = await deadline(
      searching.result,
      "explicit hybrid CLI ignored caller cancellation",
    );
    assert.equal(cancelled.code, 1, cancelled.stderr);
    assert.equal(cancelled.signal, null);
    assert.match(cancelled.stderr, /Operation cancelled by user/);
    assert.doesNotMatch(
      cancelled.stderr,
      /preparation budget|text matches only/,
    );
    assert.doesNotMatch(cancelled.stdout, /CLI_POOL_CURRENT|No .*matches/);
    await deadline(
      rig.aborted.promise,
      "CLI cancellation did not reach the model",
    );
    assert.equal(rig.queryCalls[0].signal?.aborted, true);
    assert.equal(rig.released, false);

    rig.holdQueries = false;
    const recovered = await deadline(
      rig.run([query]).result,
      "daemon could not serve a query after CLI cancellation",
    );
    assertSuccessful(recovered);
    assert.match(recovered.stdout, /CLI_POOL_CURRENT/);
    assert.doesNotMatch(recovered.stderr, /preparation budget|incomplete/);
  },
);

async function createRig(t, { source = currentSource, budgetMs } = {}) {
  const temporary = await createTemporaryDirectory(t, "zvec-cli-budget-", {
    cleanup: false,
  });
  const root = join(temporary, "repo");
  const home = join(temporary, "home");
  const entered = Promise.withResolvers();
  const aborted = Promise.withResolvers();
  const release = Promise.withResolvers();
  const children = [];
  const queryCalls = [];
  const responses = [];
  let backend;
  let server;
  let instance;
  const rig = {
    root,
    entered,
    aborted,
    queryCalls,
    responses,
    holdQueries: true,
    released: false,
  };
  t.after(async () => {
    rig.released = true;
    release.resolve();
    for (const request of children) {
      if (!request.settled) request.child.kill("SIGTERM");
    }
    try {
      await deadline(
        Promise.allSettled(children.map((request) => request.result)),
        "CLI children did not terminate during cleanup",
        5_000,
      );
    } finally {
      for (const request of children) {
        if (!request.settled) request.child.kill("SIGKILL");
      }
      try {
        await server?.close();
      } finally {
        try {
          await backend?.close();
        } finally {
          // This lock belongs to the test process, so never use --server off.
          await instance?.release();
          await removeTemporaryDirectory(temporary);
        }
      }
    }
  });
  await mkdir(root);
  await writeFile(join(root, "pool.ts"), source);
  const service = await createZvecGrep({
    root,
    embeddingModel: new FakeLocalModel(),
  });
  try {
    await service.index();
  } finally {
    await service.close();
  }
  class HeldModel extends FakeLocalModel {
    async doEmbed(contents, options) {
      if (options.purpose === "query") {
        queryCalls.push({ contents, signal: options.signal });
        if (rig.holdQueries) {
          entered.resolve();
          try {
            await waitForAbortOrRelease(options.signal, release.promise);
          } finally {
            if (options.signal?.aborted) aborted.resolve();
          }
        }
      }
      return super.doEmbed(contents, options);
    }
  }
  backend = new DaemonBackend({
    version: "1.0.0",
    ...(budgetMs === undefined
      ? {}
      : { semanticPreparationBudgetMs: budgetMs }),
    watchManagerFactory: () => ({
      start() {},
      flushPending: async () => {},
      close: async () => {},
    }),
    modelPoolOptions: { createModel: () => new HeldModel() },
  });
  const search = backend.search.bind(backend);
  backend.search = async (...args) => {
    const response = await search(...args);
    responses.push(response);
    return response;
  };
  server = new DaemonHttpServer({
    host: "127.0.0.1",
    port: 0,
    version: "1.0.0",
    backend,
  });
  const address = await server.start();
  const serverUrl = `http://127.0.0.1:${address.port}/mcp`;
  instance = await DaemonInstanceLock.acquire(home, serverUrl);
  await instance.markReady();
  const env = Object.fromEntries(
    Object.entries(process.env).filter(
      ([name]) => !name.startsWith("ZVEC_GREP_"),
    ),
  );
  Object.assign(env, {
    HOME: home,
    USERPROFILE: home,
    ZVEC_GREP_HOME: home,
    ZVEC_GREP_SERVER_URL: serverUrl,
    NO_COLOR: "1",
  });
  rig.run = (args) => {
    const child = spawn(
      process.execPath,
      ["--liftoff-only", cliPath, ...args],
      {
        cwd: root,
        env,
        stdio: ["ignore", "pipe", "pipe"],
        windowsHide: true,
      },
    );
    let stdout = "";
    let stderr = "";
    const request = { child, settled: false };
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    request.result = new Promise((resolve, reject) => {
      child.once("error", reject);
      child.once("close", (code, signal) =>
        resolve({ code, signal, stdout, stderr }),
      );
    }).finally(() => {
      request.settled = true;
    });
    void request.result.catch(() => {});
    children.push(request);
    return request;
  };
  return rig;
}

function assertSuccessful(result) {
  assert.equal(result.code, 0, result.stderr);
  assert.equal(result.signal, null);
}

function assertBudgetWarning(stderr) {
  assert.ok(stderr.includes(budgetWarning), stderr);
  assert.doesNotMatch(
    stderr,
    /No index found|preparing .*index|semantic search unavailable|falling back/i,
  );
}

async function waitForAbortOrRelease(signal, released) {
  signal?.throwIfAborted();
  let abort;
  try {
    await Promise.race([
      released,
      new Promise((_, reject) => {
        abort = () => reject(signal.reason);
        signal?.addEventListener("abort", abort, { once: true });
      }),
    ]);
  } finally {
    if (abort) signal?.removeEventListener("abort", abort);
  }
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
