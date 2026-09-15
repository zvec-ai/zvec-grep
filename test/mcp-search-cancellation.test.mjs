import assert from "node:assert/strict";
import { getEventListeners } from "node:events";
import { createServer } from "node:http";
import { join } from "node:path";
import test from "node:test";
import {
  Client,
  StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client";
import {
  DaemonClient,
  DaemonCallTimeoutError,
} from "../dist/client/daemon-client.js";
import { DaemonHttpServer } from "../dist/daemon/http-server.js";
import { normalizeSearchInput } from "../dist/mcp/input-normalization.js";
import {
  zvecGrepCliSearchInputSchema,
  zvecGrepSearchInputSchema,
} from "../dist/mcp/schemas.js";
import { createTemporaryDirectory } from "./helpers/fixtures.mjs";

test("pre-aborted daemon calls do not read credentials, send requests, or attach listeners", async (t) => {
  const home = await createTemporaryDirectory(t, "zvec-preabort-search-");
  let requests = 0;
  const server = createServer((_request, response) => {
    requests += 1;
    response.end();
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const controller = new AbortController();
  controller.abort(new Error("fixture cancellation"));
  const sigintListeners = process.listenerCount("SIGINT");
  const client = new DaemonClient({
    serverUrl: `http://127.0.0.1:${server.address().port}/mcp`,
    home,
    tokenFile: join(home, "must-not-read-missing-token"),
  });
  try {
    await assert.rejects(
      client.callTool(
        "zvec_grep_search",
        { root: home, query: "connection pool" },
        { signal: controller.signal },
      ),
      /Operation cancelled by user/,
    );
    assert.equal(requests, 0);
    assert.equal(process.listenerCount("SIGINT"), sigintListeners);
    assert.equal(getEventListeners(controller.signal, "abort").length, 0);
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
});

test("external daemon cancellation interrupts a stalled handshake and cleans up listeners", async (t) => {
  const home = await createTemporaryDirectory(t, "zvec-handshake-cancel-");
  const entered = deferred();
  const server = createServer(() => entered.resolve());
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const client = new DaemonClient({
    serverUrl: `http://127.0.0.1:${server.address().port}/mcp`,
    home,
  });
  const controller = new AbortController();
  const sigintListeners = process.listenerCount("SIGINT");
  const request = client.callTool(
    "zvec_grep_search",
    { root: home, query: "connection pool" },
    { signal: controller.signal, timeoutMs: 5_000 },
  );
  void request.catch(() => {});
  try {
    await deadline(entered.promise, "HTTP handshake was not reached");
    controller.abort(new Error("fixture request cancelled"));
    await assert.rejects(
      deadline(request, "client ignored cancellation"),
      (error) => {
        assert.equal(error instanceof DaemonCallTimeoutError, false);
        assert.match(error.message, /Operation cancelled by user/);
        return true;
      },
    );
    assert.equal(process.listenerCount("SIGINT"), sigintListeners);
    assert.equal(getEventListeners(controller.signal, "abort").length, 0);
  } finally {
    controller.abort();
    server.closeAllConnections();
    await Promise.allSettled([request]);
    await new Promise((resolve) => server.close(resolve));
  }
});

for (const cancellation of ["external", "deadline"]) {
  test(`daemon ${cancellation} cancellation reaches the held backend search`, async (t) => {
    const home = await createTemporaryDirectory(t, "zvec-search-abort-");
    const held = heldSearch();
    const server = new DaemonHttpServer({
      host: "127.0.0.1",
      port: 0,
      version: "test",
      backend: { search: held.search },
    });
    const address = await server.start();
    const client = new DaemonClient({
      serverUrl: `http://127.0.0.1:${address.port}/mcp`,
      home,
    });
    const controller = new AbortController();
    const sigintListeners = process.listenerCount("SIGINT");
    const request = client.callTool(
      "zvec_grep_search",
      { root: home, query: "connection pool", autoUpdate: false },
      {
        signal: controller.signal,
        timeoutMs: cancellation === "deadline" ? 1_000 : 5_000,
      },
    );
    void request.catch(() => {});
    try {
      const signal = await deadline(
        held.entered.promise,
        "backend was not reached",
      );
      assert.ok(signal instanceof AbortSignal);
      assert.equal(signal.aborted, false);
      if (cancellation === "external") {
        controller.abort(new Error("fixture user cancellation"));
      }
      await assert.rejects(
        deadline(request, "client cancellation did not settle"),
        (error) => {
          assert.equal(
            error instanceof DaemonCallTimeoutError,
            cancellation === "deadline",
          );
          assert.match(
            error.message,
            cancellation === "deadline"
              ? /time budget/
              : /Operation cancelled by user/,
          );
          return true;
        },
      );
      await deadline(
        held.aborted.promise,
        "MCP did not propagate cancellation to backend",
      );
      await deadline(
        held.settled.promise,
        "backend did not release its held request",
      );
      assert.equal(signal.aborted, true);
      assert.equal(process.listenerCount("SIGINT"), sigintListeners);
      assert.equal(getEventListeners(controller.signal, "abort").length, 0);
      const nextController = new AbortController();
      const next = await client.callTool(
        "zvec_grep_search",
        { root: home, query: "next request", autoUpdate: false },
        { signal: nextController.signal },
      );
      assert.equal(
        next.freshness,
        "fresh",
        "cancelling a request must not stop the server",
      );
      assert.equal(getEventListeners(nextController.signal, "abort").length, 0);
    } finally {
      controller.abort();
      held.release.resolve();
      await Promise.allSettled([request]);
      await server.close();
    }
  });
}

test("public MCP cancellation propagates through a legacy HTTP session", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-legacy-search-abort-");
  const held = heldSearch();
  const server = new DaemonHttpServer({
    host: "127.0.0.1",
    port: 0,
    version: "test",
    backend: { search: held.search },
  });
  const address = await server.start();
  const client = new Client(
    { name: "legacy-search-cancel", version: "test" },
    { versionNegotiation: { mode: "legacy" } },
  );
  const controller = new AbortController();
  let request;
  try {
    await client.connect(
      new StreamableHTTPClientTransport(
        new URL(`http://127.0.0.1:${address.port}/mcp`),
      ),
    );
    request = client.callTool(
      {
        name: "zvec_grep_search",
        arguments: { root, query: "connection pool" },
      },
      { signal: controller.signal },
    );
    void request.catch(() => {});
    const signal = await deadline(
      held.entered.promise,
      "legacy backend was not reached",
    );
    assert.ok(signal instanceof AbortSignal);
    controller.abort(new Error("legacy caller cancelled"));
    await assert.rejects(
      deadline(request, "legacy caller ignored cancellation"),
    );
    await deadline(
      held.aborted.promise,
      "legacy MCP did not forward cancellation",
    );
    await deadline(held.settled.promise, "legacy backend remained held");
    assert.equal(signal.aborted, true);
  } finally {
    controller.abort();
    held.release.resolve();
    await Promise.allSettled([request]);
    await client.close();
    await server.close();
  }
});

test("public MCP text exposes semantic budget incompleteness without claiming no matches", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-semantic-warning-");
  const server = new DaemonHttpServer({
    host: "127.0.0.1",
    port: 0,
    version: "test",
    backend: {
      search: async () =>
        searchResponse(root, {
          emptyReason: "semantic_incomplete",
          semantic: {
            status: "skipped",
            reason: "preparation_budget_exceeded",
            budgetMs: 1_000,
          },
        }),
    },
  });
  const address = await server.start();
  const client = new Client(
    { name: "semantic-warning", version: "test" },
    { versionNegotiation: { mode: { pin: "2026-07-28" } } },
  );
  try {
    await client.connect(
      new StreamableHTTPClientTransport(
        new URL(`http://127.0.0.1:${address.port}/mcp`),
      ),
    );
    const result = await client.callTool({
      name: "zvec_grep_search",
      arguments: { root, query: "connection pool" },
    });
    assert.equal(result.isError, undefined);
    assert.equal(result.structuredContent, undefined);
    const text = result.content.map((item) => item.text ?? "").join("\n");
    assert.match(
      text,
      /warning: semantic search exceeded the preparation budget/,
    );
    assert.match(text, /search coverage is incomplete/);
    assert.match(text, /No local text matches; semantic search exceeded/);
    assert.doesNotMatch(text, /(?:^|\n)No matches\./);
  } finally {
    await client.close();
    await server.close();
  }
});

test("explicit semantic wait is normalized only from the internal CLI schema", () => {
  const input = {
    root: "/repo",
    query: "connection pool",
    semanticPolicy: "wait",
  };
  const publicInput = normalizeSearchInput(
    zvecGrepSearchInputSchema.parse(input),
  );
  assert.equal("semanticPolicy" in publicInput, false);
  const internalInput = normalizeSearchInput(
    zvecGrepCliSearchInputSchema.parse(input),
  );
  assert.equal(internalInput.semanticPolicy, "wait");
  assert.deepEqual(internalInput.queries, ["connection pool"]);
  assert.deepEqual(internalInput.routes, []);
  assert.throws(() =>
    zvecGrepCliSearchInputSchema.parse({ ...input, semanticPolicy: "other" }),
  );
});

function heldSearch() {
  const entered = deferred();
  const aborted = deferred();
  const settled = deferred();
  const release = deferred();
  let calls = 0;
  return {
    entered,
    aborted,
    settled,
    release,
    async search(input, options) {
      if (++calls > 1) return searchResponse(input.root);
      const signal = options?.signal;
      entered.resolve(signal);
      let onAbort;
      try {
        await Promise.race([
          release.promise,
          new Promise((_, reject) => {
            onAbort = () => {
              aborted.resolve();
              reject(signal.reason ?? new Error("backend request cancelled"));
            };
            if (signal?.aborted) onAbort();
            else signal?.addEventListener("abort", onAbort, { once: true });
          }),
        ]);
        return searchResponse(input.root);
      } finally {
        signal?.removeEventListener("abort", onAbort);
        settled.resolve();
      }
    },
  };
}

function searchResponse(root, diagnostics = {}) {
  return {
    root,
    freshness: "fresh",
    result: {
      root,
      query: "connection pool",
      source: "index",
      coverage: "ranked_sample",
      items: [],
      diagnostics,
    },
  };
}

function deferred() {
  let resolve;
  const promise = new Promise((fulfill) => {
    resolve = fulfill;
  });
  return { promise, resolve };
}

async function deadline(promise, message, timeoutMs = 3_000) {
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
