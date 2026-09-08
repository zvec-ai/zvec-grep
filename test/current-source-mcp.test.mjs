import assert from "node:assert/strict";
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
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
import {
  Client,
  StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client";
import { DaemonBackend } from "../dist/daemon/backend.js";
import { DaemonHttpServer } from "../dist/daemon/http-server.js";
import { writeWorkspaceManifest } from "../dist/engine/manifest.js";
import { FakeEmbeddingModel } from "./helpers/fake-embedding.mjs";

const query = "where does permission validation occur";
const marker = "COLD_KEYWORD_MARKER";
const source = `// permission validation ${marker}\nexport function verifyToken(token) { return Boolean(token); }\n`;
const token = "current-source-mcp-test-token-at-least-32-characters";

test("cold default MCP search returns current keyword source without an index, model or job", async (t) => {
  const rig = await fixture(t);
  const tools = await rig.agent.listTools();
  assert.deepEqual(
    tools.tools.map((tool) => tool.name),
    ["zvec_grep_search"],
  );
  const response = await rig.agent.callTool(
    { name: "zvec_grep_search", arguments: { root: rig.root, query } },
    { timeoutMs: 5_000 },
  );
  assert.equal(response.isError, undefined, responseText(response));
  assert.equal(
    response.structuredContent,
    undefined,
    "the default agent endpoint stays compact",
  );
  const text = responseText(response);
  assert.match(text, /auth\.ts/);
  assert.match(text, /matchedBy=keyword/);
  assert.ok(text.includes(marker), text);
  assert.match(text, /freshness: fresh/);
  assert.match(text, /semantic.*(?:unavailable|not ready)/i);
  assert.match(text, /incomplete/i);
  assert.doesNotMatch(
    text,
    /No matches\.|INDEX_MISSING|served_from_current_index/,
  );
  await rig.assertNoIndexActivity();
});

test("cold admin MCP search preserves current-source items and semantic coverage diagnostics", async (t) => {
  const rig = await fixture(t);
  const response = await rig.admin.callTool({
    name: "zvec_grep_search",
    arguments: { root: rig.root, query },
  });
  const data = structuredSearch(response);
  assert.equal(data.root, rig.root);
  assert.equal(data.freshness, "fresh");
  assert.equal(data.indexing, undefined);
  assert.equal(data.result.root, rig.root);
  assert.equal(data.result.source, "rg");
  assert.equal(data.result.workspaceIndex, undefined);
  assert.ok(
    data.result.items.length > 0,
    "current-source items must not be stripped as duplicate index groups",
  );
  assert.ok(
    data.result.items.some(
      (item) => item.matchedBy === "keyword" && item.content.includes(marker),
    ),
  );
  assert.ok(data.result.items.every((item) => item.status === "fresh"));
  assert.deepEqual(data.result.diagnostics.semantic, {
    status: "skipped",
    reason: "index_unavailable",
  });
  await rig.assertNoIndexActivity();
});

test("cold MCP source results reread edits, additions and deletions without persistent state", async (t) => {
  const rig = await fixture(t);
  const search = async () =>
    structuredSearch(
      await rig.admin.callTool({
        name: "zvec_grep_search",
        arguments: { root: rig.root, query },
      }),
    );
  const first = await search();
  assert.ok(first.result.items.some((item) => item.content.includes(marker)));
  await writeFile(
    join(rig.root, "auth.ts"),
    "// permission validation CURRENT_EDIT_MARKER\n",
  );
  await writeFile(
    join(rig.root, "added.ts"),
    "// permission validation CURRENT_ADDED_MARKER\n",
  );
  const changed = await search();
  const changedSource = changed.result.items
    .map((item) => item.content)
    .join("\n");
  assert.match(changedSource, /CURRENT_EDIT_MARKER/);
  assert.match(changedSource, /CURRENT_ADDED_MARKER/);
  assert.ok(!changedSource.includes(marker));
  assert.equal(changed.freshness, "fresh");
  assert.ok(changed.result.items.every((item) => item.status === "fresh"));
  await unlink(join(rig.root, "auth.ts"));
  await unlink(join(rig.root, "added.ts"));
  const deleted = await search();
  assert.deepEqual(deleted.result.items, []);
  assert.equal(deleted.result.diagnostics.emptyReason, "semantic_incomplete");
  assert.deepEqual(deleted.result.diagnostics.semantic, {
    status: "skipped",
    reason: "index_unavailable",
  });
  await rig.assertNoIndexActivity();
});

test("cold MCP search honors explicit subdirectory, path, type and modification-time filters", async (t) => {
  const rig = await fixture(t);
  const scope = join(rig.root, "scope");
  // Enable ordinary rg .gitignore discovery without creating a Git index.
  await mkdir(join(scope, ".git"), { recursive: true });
  await mkdir(join(scope, ".hidden-dir"));
  await writeFile(join(scope, ".gitignore"), "ignored.ts\n");
  for (const [file, evidence, modified] of [
    ["old.ts", "OLD_ALLOWED_MARKER", 1_600_000_000],
    ["new.ts", "NEW_ALLOWED_MARKER", 1_700_000_000],
    ["excluded.ts", "GLOB_EXCLUDED_MARKER", 1_700_000_000],
    ["ignored.ts", "IGNORE_EXCLUDED_MARKER", 1_700_000_000],
    ["notes.md", "TYPE_EXCLUDED_MARKER", 1_700_000_000],
    [".hidden.ts", "ROOT_HIDDEN_TYPE_MARKER", 1_700_000_000],
    [".hidden-dir/nested.ts", "HIDDEN_DIRECTORY_MARKER", 1_700_000_000],
  ]) {
    const path = join(scope, file);
    await writeFile(path, `// permission validation ${evidence}\n`);
    await utimes(path, modified, modified);
  }
  const args = {
    root: scope,
    query,
    globs: ["!excluded.ts"],
    fileTypes: ["ts"],
  };
  const after = structuredSearch(
    await rig.admin.callTool({
      name: "zvec_grep_search",
      arguments: { ...args, modifiedAfter: "2023-01-01T00:00:00Z" },
    }),
  );
  assert.equal(after.root, scope);
  assert.equal(after.result.root, scope);
  // Native rg --type ts includes matching root-level hidden files, but does
  // not descend into hidden directories unless --hidden is also requested.
  assertPaths(after, [".hidden.ts", "new.ts"]);
  assert.match(
    after.result.items.map((item) => item.content).join("\n"),
    /NEW_ALLOWED_MARKER/,
  );
  const hidden = structuredSearch(
    await rig.admin.callTool({
      name: "zvec_grep_search",
      arguments: {
        ...args,
        hidden: true,
        modifiedAfter: "2023-01-01T00:00:00Z",
      },
    }),
  );
  assertPaths(hidden, [".hidden-dir/nested.ts", ".hidden.ts", "new.ts"]);
  assert.match(
    hidden.result.items.map((item) => item.content).join("\n"),
    /HIDDEN_DIRECTORY_MARKER/,
  );
  const withoutType = structuredSearch(
    await rig.admin.callTool({
      name: "zvec_grep_search",
      arguments: {
        root: scope,
        query,
        globs: args.globs,
        modifiedAfter: "2023-01-01T00:00:00Z",
      },
    }),
  );
  assertPaths(withoutType, ["new.ts", "notes.md"]);
  const before = structuredSearch(
    await rig.admin.callTool({
      name: "zvec_grep_search",
      arguments: { ...args, modifiedBefore: "2023-01-01T00:00:00Z" },
    }),
  );
  assertPaths(before, ["old.ts"]);
  assert.match(
    before.result.items.map((item) => item.content).join("\n"),
    /OLD_ALLOWED_MARKER/,
  );
  await assert.rejects(stat(join(scope, ".zvec-grep")), { code: "ENOENT" });
  await rig.assertNoIndexActivity();

  function assertPaths(data, expected) {
    assert.deepEqual(
      [
        ...new Set(data.result.items.map((item) => item.file.relativePath)),
      ].sort(),
      expected,
      JSON.stringify(
        data.result.items.map((item) => ({
          path: item.file.relativePath,
          content: item.content,
        })),
      ),
    );
  }
});

test("cold empty MCP searches report incomplete semantics instead of definitive no matches", async (t) => {
  const rig = await fixture(t);
  const args = {
    root: rig.root,
    query: "where are zebra satellites configured",
  };
  const agent = await rig.agent.callTool({
    name: "zvec_grep_search",
    arguments: args,
  });
  assert.equal(agent.isError, undefined);
  const text = responseText(agent);
  assert.match(text, /No local text matches/i);
  assert.match(text, /semantic.*(?:unavailable|not ready)/i);
  assert.match(text, /incomplete/i);
  assert.doesNotMatch(text, /No matches\.|INDEX_MISSING/);
  const admin = structuredSearch(
    await rig.admin.callTool({ name: "zvec_grep_search", arguments: args }),
  );
  assert.equal(admin.result.source, "rg");
  assert.deepEqual(admin.result.items, []);
  assert.equal(admin.result.diagnostics.emptyReason, "semantic_incomplete");
  assert.deepEqual(admin.result.diagnostics.semantic, {
    status: "skipped",
    reason: "index_unavailable",
  });
  await rig.assertNoIndexActivity();
});

for (const [label, args] of [
  ["freshness wait", { query, freshness: "wait_for_fresh" }],
  ["explicit vector", { vector: query }],
]) {
  test(`cold public MCP ${label} does not silently substitute current-source recall`, async (t) => {
    const rig = await fixture(t);
    const response = await rig.agent.callTool({
      name: "zvec_grep_search",
      arguments: { root: rig.root, ...args },
    });
    assert.equal(response.isError, true);
    assert.match(responseText(response), /INDEX_MISSING/);
    assert.doesNotMatch(
      responseText(response),
      /matchedBy=keyword|COLD_KEYWORD_MARKER/,
    );
    await rig.assertNoIndexActivity();
  });
}

test("disabled MCP workspace searches current files without rewriting its policy or building an index", async (t) => {
  const rig = await fixture(t);
  const home = join(rig.root, ".zvec-grep");
  writeWorkspaceManifest(home, {
    manifestVersion: 1,
    id: "disabled-fixture",
    name: "disabled-fixture",
    path: home,
    rootPaths: [{ absolutePath: rig.root, recursive: true }],
    indexPolicy: "disabled",
    embedding: null,
    indexVersion: null,
    createdTime: 1,
    updatedTime: 1,
    embeddingRuntime: {},
  });
  const manifest = await readFile(join(home, "manifest.json"), "utf8");
  const result = structuredSearch(
    await rig.admin.callTool({
      name: "zvec_grep_search",
      arguments: { root: rig.root, query },
    }),
  );
  assert.equal(result.result.source, "rg");
  assert.ok(
    result.result.items.some(
      (item) => item.matchedBy === "keyword" && item.content.includes(marker),
    ),
  );
  assert.deepEqual(result.result.diagnostics.semantic, {
    status: "skipped",
    reason: "index_unavailable",
  });
  await rig.assertNoIndexActivity({ manifest });
});

test("cold MCP recall remains available while a real initial index job holds model loading", async (t) => {
  const entered = deferred();
  const released = deferred();
  let modelReleased = false;
  let writerCalls = 0;
  let writerCloses = 0;
  const rig = await fixture(t, {
    allowIndex: true,
    cleanup: () => released.resolve(),
    createModel: async (_request, createDefaultModel) => {
      entered.resolve();
      await released.promise;
      modelReleased = true;
      return createDefaultModel();
    },
    createService: async () => ({
      // The gate is a real backend model acquisition. Only the post-release
      // writer is fake, so this transport test never opens native storage.
      index: async () => {
        writerCalls += 1;
        return {};
      },
      close: async () => {
        writerCloses += 1;
      },
    }),
  });
  try {
    const indexing = await rig.backend.index({
      root: rig.root,
      embedding: "local/potion-code-16m-v2",
      wait: false,
    });
    await bounded(entered.promise, "initial index did not reach model loading");
    const before = { ...rig.calls };
    assert.equal(before.modelLoads, 1);
    assert.equal(before.index, 1);
    assert.equal(before.submitted, 1);
    assert.equal(rig.backend.scheduler.get(indexing.jobId).state, "running");
    const response = await rig.agent.callTool(
      {
        name: "zvec_grep_search",
        arguments: { root: rig.root, query },
      },
      { timeoutMs: 3_000 },
    );
    assert.equal(response.isError, undefined, responseText(response));
    assert.match(responseText(response), /matchedBy=keyword/);
    assert.ok(responseText(response).includes(marker));
    assert.equal(
      modelReleased,
      false,
      "current source returned before the held model load",
    );
    assert.equal(writerCalls, 0);
    assert.equal(writerCloses, 0);
    assert.deepEqual(
      rig.calls,
      before,
      "search must not submit or join another model/index operation",
    );
    assert.equal(rig.backend.scheduler.getByRoot(rig.root).id, indexing.jobId);
    assert.equal(rig.backend.scheduler.get(indexing.jobId).state, "running");
  } finally {
    released.resolve();
    await bounded(
      rig.backend.scheduler.waitForRootIdle(rig.root),
      "released initial job did not drain",
    );
  }
  assert.equal(writerCalls, 1);
  assert.equal(writerCloses, 1);
  assert.equal(rig.backend.modelPool.snapshot().activeLeases, 0);
  // A no-op writer deliberately leaves no index schema; this test proves
  // transport availability during preparation, not successful native indexing.
  const terminal = rig.backend.scheduler.getByRoot(rig.root);
  assert.equal(terminal.state, "failed");
  assert.equal(terminal.error.code, "INDEX_MISSING");
});

test("cold MCP caller cancellation reaches the actual current-source search and drains its metadata work", async (t) => {
  const entered = deferred();
  const released = deferred();
  const serverAborted = deferred();
  const serverSettled = deferred();
  const controller = new AbortController();
  const rig = await fixture(t, { cleanup: () => released.resolve() });
  const inspectRoot = rig.backend.inspectRoot.bind(rig.backend);
  let gateNextSourceMetadata = true;
  t.mock.method(rig.backend, "inspectRoot", async (...args) => {
    const info = await inspectRoot(...args);
    // Authorization planning uses the same metadata method before a source
    // search owns any work. Only pause the real current-source operation.
    if (gateNextSourceMetadata && rig.backend.sourceSearches.size > 0) {
      gateNextSourceMetadata = false;
      entered.resolve();
      await released.promise;
    }
    return info;
  });
  const search = rig.backend.search.bind(rig.backend);
  let observeNextSearch = true;
  let signal;
  let outcome;
  t.mock.method(rig.backend, "search", async (input, options) => {
    if (!observeNextSearch) return search(input, options);
    observeNextSearch = false;
    signal = options?.signal;
    const onAbort = () => serverAborted.resolve();
    signal?.addEventListener("abort", onAbort, { once: true });
    if (signal?.aborted) onAbort();
    try {
      const result = await search(input, options);
      outcome = { status: "fulfilled", result };
      return result;
    } catch (error) {
      outcome = { status: "rejected", error };
      throw error;
    } finally {
      signal?.removeEventListener("abort", onAbort);
      serverSettled.resolve();
    }
  });
  const request = rig.agent.callTool(
    { name: "zvec_grep_search", arguments: { root: rig.root, query } },
    { signal: controller.signal, timeoutMs: 15_000 },
  );
  void request.catch(() => {});
  try {
    await bounded(entered.promise, "current-source metadata was not reached");
    assert.ok(signal instanceof AbortSignal);
    assert.equal(signal.aborted, false);
    assert.equal(rig.backend.sourceSearches.size, 1);
    controller.abort(new Error("current-source caller cancelled"));
    await bounded(
      serverAborted.promise,
      "HTTP cancellation did not reach the actual backend signal",
    );
    await assert.rejects(
      bounded(request, "the cancelled MCP caller did not settle"),
    );
    assert.equal(signal.aborted, true);
    assert.equal(outcome, undefined, "the metadata gate is still held");
    assert.equal(
      rig.backend.sourceSearches.size,
      1,
      "owned metadata work must not detach on caller cancellation",
    );
    released.resolve();
    await bounded(
      serverSettled.promise,
      "the actual cancelled source search did not drain",
    );
    assert.equal(
      outcome.status,
      "rejected",
      "cancellation must not turn into a current-source result",
    );
    assert.equal(outcome.error, signal.reason);
    assert.equal(rig.backend.sourceSearches.size, 0);
    await rig.assertNoIndexActivity();

    const next = await rig.agent.callTool(
      { name: "zvec_grep_search", arguments: { root: rig.root, query } },
      { timeoutMs: 5_000 },
    );
    assert.equal(next.isError, undefined, responseText(next));
    assert.match(responseText(next), /matchedBy=keyword/);
    assert.ok(responseText(next).includes(marker));
    assert.equal(rig.backend.sourceSearches.size, 0);
    await rig.assertNoIndexActivity();
  } finally {
    controller.abort();
    released.resolve();
    await bounded(
      Promise.allSettled([request]),
      "cancelled client cleanup did not settle",
    );
    if (signal) {
      await bounded(
        serverSettled.promise,
        "server search cleanup did not settle",
      );
    }
  }
});

async function fixture(t, options = {}) {
  const temporary = await mkdtemp(join(tmpdir(), "zvec-current-source-mcp-"));
  const path = join(temporary, "repo");
  await mkdir(path);
  const root = await realpath(path);
  await writeFile(join(root, "auth.ts"), source);
  const calls = {
    modelLoads: 0,
    queryEmbeddings: 0,
    documentEmbeddings: 0,
    index: 0,
    submitted: 0,
  };
  class FixtureModel extends FakeEmbeddingModel {
    info = {
      ...this.info,
      provider: "local",
      name: "potion-code-16m-v2",
      reference: "local/potion-code-16m-v2",
    };
    async doEmbed(contents, options) {
      if (options.purpose === "document") calls.documentEmbeddings += 1;
      else calls.queryEmbeddings += 1;
      return super.doEmbed(contents, options);
    }
  }
  const backend = new DaemonBackend({
    version: "test",
    modelPoolOptions: {
      createModel: (request) => {
        calls.modelLoads += 1;
        return options.createModel
          ? options.createModel(request, () => new FixtureModel())
          : new FixtureModel();
      },
    },
    watchManagerFactory: () => ({
      start() {},
      flushPending: async () => {},
      close: async () => {},
    }),
    createService:
      options.createService ??
      (async () =>
        assert.fail("cold source search must not create an index writer")),
  });
  const index = backend.index.bind(backend);
  t.mock.method(backend, "index", async (...args) => {
    calls.index += 1;
    assert.equal(
      options.allowIndex,
      true,
      "a search-only MCP request must not create a persistent index",
    );
    return index(...args);
  });
  const submit = backend.scheduler.submit.bind(backend.scheduler);
  t.mock.method(backend.scheduler, "submit", (input) => {
    calls.submitted += 1;
    return submit(input);
  });
  const server = new DaemonHttpServer({
    host: "127.0.0.1",
    port: 0,
    token,
    version: "test",
    backend,
  });
  const clients = [];
  t.after(async () => {
    await options.cleanup?.();
    await Promise.allSettled(clients.map((client) => client.close()));
    try {
      await server.close();
    } finally {
      try {
        await backend.close();
      } finally {
        await rm(temporary, { recursive: true, force: true });
      }
    }
  });
  const address = await server.start();
  async function connect(endpoint) {
    const client = new Client(
      { name: `current-source-${endpoint}`, version: "test" },
      { versionNegotiation: { mode: { pin: "2026-07-28" } } },
    );
    clients.push(client);
    await client.connect(
      new StreamableHTTPClientTransport(
        new URL(`http://127.0.0.1:${address.port}/${endpoint}`),
        { requestInit: { headers: { Authorization: `Bearer ${token}` } } },
      ),
    );
    return client;
  }
  return {
    root,
    backend,
    calls,
    agent: await connect("mcp"),
    admin: await connect("mcp/admin"),
    async assertNoIndexActivity({ manifest } = {}) {
      assert.deepEqual(calls, {
        modelLoads: 0,
        queryEmbeddings: 0,
        documentEmbeddings: 0,
        index: 0,
        submitted: 0,
      });
      assert.deepEqual(backend.scheduler.snapshot(), { queued: 0, running: 0 });
      assert.equal(backend.scheduler.getByRoot(root), undefined);
      assert.equal(backend.runtimeManager.snapshot().activeRuntimes, 0);
      assert.deepEqual(backend.modelPool.snapshot(), {
        loaded: 0,
        activeLeases: 0,
      });
      if (manifest === undefined) {
        await assert.rejects(stat(join(root, ".zvec-grep")), {
          code: "ENOENT",
        });
      } else {
        assert.equal(
          await readFile(join(root, ".zvec-grep", "manifest.json"), "utf8"),
          manifest,
        );
        assert.deepEqual(await readdir(join(root, ".zvec-grep")), [
          "manifest.json",
        ]);
      }
    },
  };
}

function responseText(response) {
  return response.content
    .filter((item) => item.type === "text")
    .map((item) => item.text)
    .join("\n");
}

function structuredSearch(response) {
  assert.equal(response.isError, undefined, responseText(response));
  assert.ok(
    response.structuredContent,
    "admin search must retain structured output",
  );
  assert.ok(response.structuredContent.result);
  return response.structuredContent;
}

function deferred() {
  let resolve;
  const promise = new Promise((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

async function bounded(promise, message) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(message)), 5_000);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}
