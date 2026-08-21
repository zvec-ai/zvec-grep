import assert from "node:assert/strict";
import {
  access,
  mkdir,
  mkdtemp,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { request as httpRequest } from "node:http";
import { join } from "node:path";
import test from "node:test";
import {
  Client,
  StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client";
import { DaemonBackend } from "../dist/daemon/backend.js";
import { DaemonHttpServer } from "../dist/daemon/http-server.js";
import { BaseEmbeddingModel } from "../dist/engine/models/embeddings.js";
import { createZvecGrep } from "../dist/index.js";
import { DaemonClient } from "../dist/client/daemon-client.js";

const token = "server-http-test-token-at-least-32-characters";

function assertAdminSearchStructured(search) {
  assert.ok(search.structuredContent);
  assert.ok(Array.isArray(search.structuredContent.result.groupResults));
}

test("HTTP server rolls back state after a listen failure", async () => {
  const backend = {};
  const first = new DaemonHttpServer({
    host: "127.0.0.1",
    port: 0,
    token,
    version: "1.0.0",
    backend,
  });
  const firstAddress = await first.start();
  const second = new DaemonHttpServer({
    host: "127.0.0.1",
    port: firstAddress.port,
    token,
    version: "1.0.0",
    backend,
  });
  await assert.rejects(second.start(), /EADDRINUSE|address already in use/i);
  await first.close();
  const secondAddress = await second.start();
  assert.equal(secondAddress.port, firstAddress.port);
  await second.close();
});

test("Streamable HTTP serves health, MCP contracts and a real cached index search", async (t) => {
  const temporaryDirectory = await mkdtemp(join(tmpdir(), "zvec-grep-http-"));
  const root = join(temporaryDirectory, "repo");
  await mkdir(join(root, "src"), { recursive: true });
  await writeFile(
    join(root, "src", "answer.ts"),
    ["export function answerToEverything() {", "  return 42;", "}", ""].join(
      "\n",
    ),
  );
  const canonicalRoot = await realpath(root);

  const indexModel = new TestEmbeddingModel();
  const service = await createZvecGrep({ root, embeddingModel: indexModel });
  await service.index();
  await service.close();

  let modelLoads = 0;
  let blockEmbedding = false;
  let releaseEmbedding;
  const embeddingReleased = new Promise((resolve) => {
    releaseEmbedding = resolve;
  });
  const backend = new DaemonBackend({
    version: "1.0.0",
    serviceOptions: {
      authorizationSigningKeyPath: join(temporaryDirectory, "auth.key"),
    },
    modelPoolOptions: {
      createModel: () => {
        modelLoads += 1;
        return new TestEmbeddingModel(async () => {
          if (blockEmbedding) {
            await embeddingReleased;
          }
        });
      },
    },
    readSessionIdleTtlMs: 60_000,
  });
  const server = new DaemonHttpServer({
    host: "127.0.0.1",
    port: 0,
    token,
    version: "1.0.0",
    backend,
  });
  const address = await server.start();
  const mcpUrl = new URL(`http://127.0.0.1:${address.port}/mcp`);
  const adminMcpUrl = new URL("/mcp/admin", mcpUrl);
  await mkdir(join(temporaryDirectory, "daemon"));
  await writeFile(join(temporaryDirectory, "daemon", "token"), `${token}\n`);
  t.after(async () => {
    releaseEmbedding?.();
    await server.close();
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  const health = await fetch(`http://127.0.0.1:${address.port}/healthz`);
  assert.equal(health.status, 200);
  assert.deepEqual(await health.json(), { status: "ok" });
  const cliStatus = await new DaemonClient({
    serverUrl: mcpUrl.href,
    home: temporaryDirectory,
  }).callTool("zvec_grep_server_status", {});
  assert.equal(cliStatus.version, "1.0.0");

  const unauthorized = await fetch(mcpUrl, { method: "POST", body: "{}" });
  assert.equal(unauthorized.status, 401);
  const unauthorizedShutdown = await fetch(
    new URL("/control/shutdown", mcpUrl),
    {
      method: "POST",
      headers: {
        Authorization: "Bearer invalid-token-value-that-is-long-enough",
      },
    },
  );
  assert.equal(unauthorizedShutdown.status, 401);
  const invalidHost = await rawRequestStatus(mcpUrl, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${token}`,
      Host: "example.com",
      "Content-Type": "application/json",
    },
    body: "{}",
  });
  assert.equal(invalidHost, 403);
  const invalidOrigin = await fetch(mcpUrl, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${token}`,
      Origin: "https://example.com",
      "Content-Type": "application/json",
    },
    body: "{}",
  });
  assert.equal(invalidOrigin.status, 403);
  const getMcp = await fetch(mcpUrl, {
    headers: { Authorization: `Bearer ${token}` },
  });
  assert.equal(getMcp.status, 405);

  const publicClient = await connectClient(mcpUrl, "public-client");
  t.after(async () => publicClient.close());
  const publicTools = await publicClient.listTools();
  assert.deepEqual(publicTools.tools.map((tool) => tool.name).toSorted(), [
    "zvec_grep_callees",
    "zvec_grep_callers",
    "zvec_grep_explore",
    "zvec_grep_impact",
    "zvec_grep_search",
  ]);
  await assert.rejects(
    publicClient.callTool({
      name: "zvec_grep_index_status",
      arguments: { root },
    }),
    (error) =>
      error?.code === -32602 &&
      /tool.*not found|not found.*tool/i.test(error.message),
  );

  const modernPublicClient = await connectClient(
    mcpUrl,
    "modern-public-client",
    undefined,
    true,
  );
  t.after(async () => modernPublicClient.close());
  const modernPublicTools = await modernPublicClient.listTools();
  assert.equal(modernPublicTools.ttlMs, 60 * 60 * 1_000);
  assert.equal(modernPublicTools.cacheScope, "private");
  assert.deepEqual(
    modernPublicTools.tools.map((tool) => tool.name).toSorted(),
    [
      "zvec_grep_callees",
      "zvec_grep_callers",
      "zvec_grep_explore",
      "zvec_grep_impact",
      "zvec_grep_search",
    ],
  );

  const legacyAdmin = await fetch(adminMcpUrl, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${token}`,
      Accept: "application/json, text/event-stream",
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: "2025-11-25",
        capabilities: {},
        clientInfo: { name: "legacy-admin-test", version: "1.0.0" },
      },
    }),
  });
  assert.equal((await legacyAdmin.json()).error.code, -32022);

  const clients = await Promise.all([
    connectClient(adminMcpUrl, "client-a"),
    connectClient(adminMcpUrl, "client-b"),
  ]);
  t.after(async () => Promise.all(clients.map((client) => client.close())));

  const listed = await clients[0].listTools();
  assert.deepEqual(listed.tools.map((tool) => tool.name).toSorted(), [
    "zvec_grep_callees",
    "zvec_grep_callers",
    "zvec_grep_explore",
    "zvec_grep_impact",
    "zvec_grep_index",
    "zvec_grep_index_drop",
    "zvec_grep_index_status",
    "zvec_grep_rg",
    "zvec_grep_search",
    "zvec_grep_server_status",
  ]);
  const coldStatus = await clients[0].callTool({
    name: "zvec_grep_server_status",
    arguments: {},
  });
  assert.equal(coldStatus.structuredContent.active_runtimes, 0);
  assert.equal(coldStatus.structuredContent.models.loaded, 0);
  const coldIndexStatus = await clients[0].callTool({
    name: "zvec_grep_index_status",
    arguments: { root },
  });
  assert.equal(coldIndexStatus.structuredContent.indexed, true);
  assert.equal(coldIndexStatus.structuredContent.runtime, undefined);
  assert.equal(
    coldIndexStatus.structuredContent.persistent.files.truncated_fragments,
    0,
  );
  const afterColdIndexStatus = await clients[0].callTool({
    name: "zvec_grep_server_status",
    arguments: {},
  });
  assert.equal(afterColdIndexStatus.structuredContent.active_runtimes, 0);
  assert.equal(afterColdIndexStatus.structuredContent.models.loaded, 0);

  const freshSearch = await clients[0].callTool({
    name: "zvec_grep_search",
    arguments: {
      root,
      query: "answer to everything",
      limit: 3,
      freshness: "wait_for_fresh",
    },
  });
  assert.equal(freshSearch.isError, undefined);
  assertAdminSearchStructured(freshSearch);
  assert.match(freshSearch.content[0].text, /^freshness: fresh$/m);
  assert.match(freshSearch.content[0].text, /src\/answer\.ts:/);

  const publicSearch = await publicClient.callTool({
    name: "zvec_grep_search",
    arguments: {
      root,
      query: "answer to everything",
      limit: 3,
      freshness: "wait_for_fresh",
    },
  });
  assert.equal(publicSearch.structuredContent, undefined);
  assert.equal(publicSearch.content[0].text, freshSearch.content[0].text);

  const searchRoots = [root, join(root, "src")];
  const searches = await Promise.all(
    clients.map((client, index) =>
      client.callTool({
        name: "zvec_grep_search",
        arguments: {
          root: searchRoots[index],
          query: "answer to everything",
          limit: 3,
        },
      }),
    ),
  );
  await backend.scheduler.waitForRootIdle(canonicalRoot);
  for (const search of searches) {
    assert.equal(search.isError, undefined);
    assertAdminSearchStructured(search);
    assert.match(search.content[0].text, /src\/answer\.ts:/);
  }
  assert.equal(modelLoads, 1);

  const status = await clients[0].callTool({
    name: "zvec_grep_server_status",
    arguments: {},
  });
  assert.equal(status.structuredContent.active_runtimes, 1);
  assert.equal(status.structuredContent.models.loaded, 1);

  await writeFile(
    join(root, "src", "answer.ts"),
    ["export function updatedAnswer() {", "  return 43;", "}", ""].join("\n"),
  );
  const refreshed = await clients[0].callTool({
    name: "zvec_grep_index",
    arguments: { root: join(root, "src"), wait: true },
  });
  assert.equal(refreshed.structuredContent.root, canonicalRoot);
  await backend.scheduler.waitForRootIdle(canonicalRoot);
  assert.equal(refreshed.structuredContent.state, "succeeded");
  const refreshedSearch = await clients[0].callTool({
    name: "zvec_grep_search",
    arguments: { root, fts: "updatedAnswer" },
  });
  assert.equal(refreshedSearch.isError, undefined);
  assertAdminSearchStructured(refreshedSearch);
  assert.match(refreshedSearch.content[0].text, /updatedAnswer/);

  const unindexedRoot = join(temporaryDirectory, "unindexed");
  await mkdir(unindexedRoot);
  const missing = await clients[0].callTool({
    name: "zvec_grep_search",
    arguments: { root: unindexedRoot, query: "query" },
  });
  assert.equal(missing.isError, true);
  assert.match(missing.content[0].text, /INDEX_MISSING/);
  assert.match(missing.content[0].text, /available exact-search fallback/);
  assert.match(missing.content[0].text, /explicit user authorization/);
  await assert.rejects(access(join(unindexedRoot, ".zvec-grep")));

  await writeFile(
    join(unindexedRoot, "new.ts"),
    "export const newlyIndexed = true;\n",
  );
  const rgSearch = await clients[0].callTool({
    name: "zvec_grep_rg",
    arguments: { root: unindexedRoot, command: "rg newlyIndexed" },
  });
  assert.equal(rgSearch.isError, undefined);
  assert.equal(rgSearch.structuredContent, undefined);
  assert.match(rgSearch.content[0].text, /^new\.ts\n {2}1:\t/);
  assert.match(rgSearch.content[0].text, /newlyIndexed/);

  blockEmbedding = true;
  const indexed = await clients[0].callTool({
    name: "zvec_grep_index",
    arguments: {
      root: unindexedRoot,
      embedding: "test/deterministic",
    },
  });
  assert.equal(indexed.isError, undefined);
  assert.match(indexed.structuredContent.state, /queued|running/);
  await waitFor(
    () =>
      backend.scheduler.get(indexed.structuredContent.job_id)?.progress
        ?.detail === "embedding new.ts",
  );

  const runningStatus = await clients[0].callTool({
    name: "zvec_grep_index_status",
    arguments: { root: unindexedRoot },
  });
  assert.equal(runningStatus.structuredContent.runtime.job_state, "running");
  assert.deepEqual(runningStatus.structuredContent.runtime.completion, {
    completed: 0,
    total: 1,
  });

  const duplicate = await clients[1].callTool({
    name: "zvec_grep_index",
    arguments: { root: unindexedRoot, embedding: "test/deterministic" },
  });
  assert.equal(duplicate.structuredContent.reused, true);
  assert.equal(
    duplicate.structuredContent.job_id,
    indexed.structuredContent.job_id,
  );

  let searchSettled = false;
  const writerSearchPromise = clients[0]
    .callTool({
      name: "zvec_grep_search",
      arguments: { root: unindexedRoot, fts: "newlyIndexed" },
    })
    .then((result) => {
      searchSettled = true;
      return result;
    });

  const waitedPromise = clients[1].callTool({
    name: "zvec_grep_index",
    arguments: { root: unindexedRoot, wait: true },
  });
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(searchSettled, true);
  const writerSearch = await writerSearchPromise;
  assert.equal(writerSearch.isError, undefined);
  assertAdminSearchStructured(writerSearch);
  assert.match(writerSearch.content[0].text, /^freshness: possibly_stale$/m);
  assert.match(
    writerSearch.content[0].text,
    /^results: served_from_current_index$/m,
  );
  const progressMatch = /^background_refresh: running \((\d+)\/(\d+)\)$/m.exec(
    writerSearch.content[0].text,
  );
  assert.ok(progressMatch);
  assert.ok(Number(progressMatch[1]) <= Number(progressMatch[2]));
  blockEmbedding = false;
  releaseEmbedding();
  const waited = await waitedPromise;
  assert.equal(waited.structuredContent.reused, true);
  assert.equal(waited.structuredContent.state, "succeeded");

  const indexStatus = await clients[0].callTool({
    name: "zvec_grep_index_status",
    arguments: { root: unindexedRoot },
  });
  assert.equal(indexStatus.isError, undefined);
  assert.equal(indexStatus.structuredContent.indexed, true);
  assert.equal(indexStatus.structuredContent.runtime.job_state, "succeeded");
  assert.ok(indexStatus.structuredContent.runtime.dirty_revision >= 1);
  assert.equal(
    indexStatus.structuredContent.runtime.indexed_revision,
    indexStatus.structuredContent.runtime.dirty_revision,
  );

  const newSearch = await clients[0].callTool({
    name: "zvec_grep_search",
    arguments: { root: unindexedRoot, query: "newly indexed" },
  });
  assert.equal(newSearch.isError, undefined);
  assertAdminSearchStructured(newSearch);
  assert.match(newSearch.content[0].text, /new\.ts:/);
  assert.equal(modelLoads, 1);

  const dropped = await clients[0].callTool({
    name: "zvec_grep_index",
    arguments: { root: unindexedRoot, drop: true },
  });
  assert.equal(dropped.isError, undefined);
  assert.equal(dropped.structuredContent.action, "drop");
  assert.equal(dropped.structuredContent.dropped, true);
  const droppedStatus = await clients[0].callTool({
    name: "zvec_grep_index_status",
    arguments: { root: unindexedRoot },
  });
  assert.equal(droppedStatus.structuredContent.indexed, false);
});

test("full MCP toolset restores all tools on the public endpoint", async (t) => {
  const server = new DaemonHttpServer({
    host: "127.0.0.1",
    port: 0,
    token,
    version: "1.0.0",
    backend: {},
    mcpToolset: "full",
  });
  const address = await server.start();
  const client = await connectClient(
    new URL(`http://127.0.0.1:${address.port}/mcp`),
    "full-toolset-client",
  );
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const listed = await client.listTools();
  assert.deepEqual(listed.tools.map((tool) => tool.name).toSorted(), [
    "zvec_grep_callees",
    "zvec_grep_callers",
    "zvec_grep_explore",
    "zvec_grep_impact",
    "zvec_grep_index",
    "zvec_grep_index_drop",
    "zvec_grep_index_status",
    "zvec_grep_rg",
    "zvec_grep_search",
    "zvec_grep_server_status",
  ]);
});

test("Streamable HTTP indexes and searches with qwen text-embedding-v4", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-qwen-http-"),
  );
  const root = join(temporaryDirectory, "repo");
  const endpoint = "https://qwen.test/embeddings";
  const originalFetch = globalThis.fetch;
  const requests = [];
  await mkdir(join(root, "src"), { recursive: true });
  await writeFile(
    join(root, "src", "answer.ts"),
    "export const answer = 42;\n",
  );
  globalThis.fetch = async (input, init) => {
    if (String(input) !== endpoint) {
      return originalFetch(input, init);
    }
    const body = JSON.parse(String(init?.body));
    const texts = Array.isArray(body.input) ? body.input : [];
    requests.push({ authorization: init?.headers?.Authorization, texts });
    return new Response(
      JSON.stringify({
        data: texts.map((_, index) => ({
          index,
          embedding: new Array(1024).fill(0.01),
        })),
      }),
      {
        status: 200,
        headers: { "Content-Type": "application/json" },
      },
    );
  };

  const backend = new DaemonBackend({
    version: "1.0.0",
    serviceOptions: {
      apiKey: "qwen-test-key",
      endpoint,
      authorizationSigningKeyPath: join(temporaryDirectory, "auth.key"),
    },
  });
  const server = new DaemonHttpServer({
    host: "127.0.0.1",
    port: 0,
    token,
    version: "1.0.0",
    backend,
  });
  const address = await server.start();
  const client = await connectClient(
    new URL(`http://127.0.0.1:${address.port}/mcp/admin`),
    "qwen-client",
    async () => ({
      action: "accept",
      content: { decision: "allow_workspace" },
    }),
  );
  t.after(async () => {
    await client.close();
    await server.close();
    await backend.close();
    globalThis.fetch = originalFetch;
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  const indexed = await client.callTool({
    name: "zvec_grep_index",
    arguments: { root, embedding: "qwen/text-embedding-v4", wait: true },
  });
  assert.equal(indexed.isError, undefined);
  assert.equal(indexed.structuredContent.state, "succeeded");
  const requestsAfterIndex = requests.length;

  const search = await client.callTool({
    name: "zvec_grep_search",
    arguments: {
      root,
      query: "where is the answer",
      freshness: "wait_for_fresh",
    },
  });
  assert.equal(search.isError, undefined);
  assertAdminSearchStructured(search);
  assert.match(search.content[0].text, /^freshness: fresh$/m);
  assert.match(search.content[0].text, /answer\.ts:/);
  assert.ok(requests.length > requestsAfterIndex);
  assert.ok(
    requests.every(
      (request) => request.authorization === "Bearer qwen-test-key",
    ),
  );

  const unsupportedRoot = join(temporaryDirectory, "unsupported");
  await mkdir(unsupportedRoot);
  const unsupported = await client.callTool({
    name: "zvec_grep_index",
    arguments: {
      root: unsupportedRoot,
      embedding: "qwen/unsupported-embedding",
      wait: true,
    },
  });
  assert.equal(unsupported.isError, undefined);
  assert.equal(unsupported.structuredContent.state, "failed");
  assert.deepEqual(unsupported.structuredContent.error, {
    code: "MODEL_LOAD_FAILED",
    message:
      "[MODEL_LOAD_FAILED] Embedding model qwen/unsupported-embedding could not be created: Embedding model is not in the zvec-grep catalog",
  });
  assert.match(unsupported.content[0].text, /qwen\/unsupported-embedding/);
  const unsupportedStatus = await client.callTool({
    name: "zvec_grep_index_status",
    arguments: { root: unsupportedRoot },
  });
  assert.equal(unsupportedStatus.structuredContent.indexed, false);
  assert.equal(
    unsupportedStatus.structuredContent.runtime.error.code,
    "MODEL_LOAD_FAILED",
  );
  await assert.rejects(access(join(unsupportedRoot, ".zvec-grep", "index")));
});

async function connectClient(url, name, onElicitation, modern = false) {
  const modernOnly = modern || new URL(url).pathname.endsWith("/mcp/admin");
  const client = new Client(
    { name, version: "1.0.0" },
    {
      ...(onElicitation ? { capabilities: { elicitation: { form: {} } } } : {}),
      ...(modernOnly
        ? { versionNegotiation: { mode: { pin: "2026-07-28" } } }
        : {}),
    },
  );
  if (onElicitation) {
    client.setRequestHandler("elicitation/create", onElicitation);
  }
  const transport = new StreamableHTTPClientTransport(url, {
    requestInit: {
      headers: { Authorization: `Bearer ${token}` },
    },
  });
  await client.connect(transport);
  return client;
}

async function waitFor(predicate) {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (predicate()) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error("Condition was not reached.");
}

async function rawRequestStatus(url, options) {
  return await new Promise((resolve, reject) => {
    const request = httpRequest(
      url,
      {
        method: options.method,
        headers: options.headers,
      },
      (response) => {
        response.resume();
        response.once("end", () => resolve(response.statusCode));
      },
    );
    request.once("error", reject);
    request.end(options.body);
  });
}

class TestEmbeddingModel extends BaseEmbeddingModel {
  info = {
    reference: "test/deterministic",
    provider: "test",
    name: "deterministic",
    dimension: 8,
    metric: "cosine",
    inputKinds: ["text"],
    limits: { maxBatchSize: 64 },
  };

  constructor(beforeEmbed = async () => {}) {
    super();
    this.beforeEmbed = beforeEmbed;
  }

  async doEmbed(contents) {
    await this.beforeEmbed();
    return {
      vectors: contents.map((content) => {
        const text = content.kind === "text" ? content.text : "";
        const vector = new Array(this.info.dimension).fill(0);
        for (let index = 0; index < text.length; index++) {
          vector[index % vector.length] += text.charCodeAt(index) / 255;
        }
        const norm =
          Math.sqrt(vector.reduce((sum, value) => sum + value * value, 0)) || 1;
        return vector.map((value) => value / norm);
      }),
      truncated: [],
    };
  }
}
