import assert from "node:assert/strict";
import {
  access,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { request as httpRequest } from "node:http";
import { join } from "node:path";
import test from "node:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { ElicitRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { DaemonBackend } from "../dist/daemon/backend.js";
import { DaemonHttpServer } from "../dist/daemon/http-server.js";
import { EmbeddingModel } from "../dist/engine/models/embeddings.js";
import { createZvecGrep } from "../dist/index.js";
import { DaemonClient } from "../dist/client/daemon-client.js";

const token = "server-http-test-token-at-least-32-characters";

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
    resolveEmbeddingSchema: () => ({
      provider: "test",
      model: "deterministic",
      dimension: 8,
      metric: "cosine",
    }),
    readCollectionIdleTtlMs: 60_000,
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

  const clients = await Promise.all([
    connectClient(mcpUrl, "client-a"),
    connectClient(mcpUrl, "client-b"),
  ]);
  t.after(async () => Promise.all(clients.map((client) => client.close())));

  const listed = await clients[0].listTools();
  assert.deepEqual(listed.tools.map((tool) => tool.name).toSorted(), [
    "zvec_grep_index",
    "zvec_grep_index_drop",
    "zvec_grep_index_status",
    "zvec_grep_remote_embedding_demo",
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
  assert.equal(freshSearch.structuredContent.freshness, "fresh");

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
    assert.equal(search.structuredContent.root, canonicalRoot);
    assert.ok(search.structuredContent.result.items.length > 0);
    assert.equal(
      search.structuredContent.result.items[0].file.relativePath,
      "src/answer.ts",
    );
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
  assert.match(
    refreshedSearch.structuredContent.result.items[0].content,
    /updatedAnswer/,
  );

  const unindexedRoot = join(temporaryDirectory, "unindexed");
  await mkdir(unindexedRoot);
  const missing = await clients[0].callTool({
    name: "zvec_grep_search",
    arguments: { root: unindexedRoot, query: "query" },
  });
  assert.equal(missing.isError, true);
  assert.match(missing.content[0].text, /INDEX_MISSING/);
  await assert.rejects(access(join(unindexedRoot, ".zvec-grep")));

  await writeFile(
    join(unindexedRoot, "new.ts"),
    "export const newlyIndexed = true;\n",
  );
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
  assert.equal(writerSearch.structuredContent.freshness, "possibly_stale");
  assert.equal(writerSearch.structuredContent.indexing.state, "running");
  assert.equal(
    typeof writerSearch.structuredContent.indexing.completed,
    "number",
  );
  assert.equal(typeof writerSearch.structuredContent.indexing.total, "number");
  assert.ok(
    writerSearch.structuredContent.indexing.completed <=
      writerSearch.structuredContent.indexing.total,
  );
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
  assert.equal(
    newSearch.structuredContent.result.items[0].file.relativePath,
    "new.ts",
  );
  assert.equal(modelLoads, 1);
});

test("Remote Embedding demo elicits once and reuses its Workspace grant", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-auth-demo-"),
  );
  const root = join(temporaryDirectory, "repo");
  await mkdir(root);
  const demoFileText = "Local workspace content for remote embedding.\n";
  await writeFile(join(root, "README.md"), demoFileText);
  await writeFile(
    join(temporaryDirectory, "outside.txt"),
    "This file must never be uploaded by the demo.\n",
  );
  let embeddingCalls = 0;
  const backend = new DaemonBackend({
    version: "1.0.0",
    serviceOptions: {
      authorizationSigningKeyPath: join(temporaryDirectory, "auth.key"),
    },
    modelPoolOptions: {
      createModel: () =>
        new TestEmbeddingModel(async () => {
          embeddingCalls += 1;
        }),
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
  const mcpUrl = new URL(`http://127.0.0.1:${address.port}/mcp`);
  let elicitations = 0;
  const client = await connectClient(
    mcpUrl,
    "remote-embedding-auth-demo",
    async (request) => {
      elicitations += 1;
      assert.equal(request.params.mode, "form");
      assert.match(
        request.params.message,
        /Send query text and selected workspace file/,
      );
      assert.match(request.params.message, /From\s+repo/);
      assert.match(request.params.message, /read only after approval/i);
      assert.equal(request.params.message.includes("\n"), true);
      assert.ok(request.params.message.length < 240);
      return {
        action: "accept",
        content: { decision: "allow_workspace" },
      };
    },
  );
  t.after(async () => {
    await client.close();
    await server.close();
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  const first = await client.callTool({
    name: "zvec_grep_remote_embedding_demo",
    arguments: {
      root,
      text: "first authorized remote query",
      filePath: "README.md",
    },
  });
  assert.equal(first.isError, undefined);
  assert.equal(first.structuredContent.authorization, "granted_workspace");
  assert.equal(first.structuredContent.query_vector_dimensions, 8);
  assert.equal(first.structuredContent.file_vector_dimensions, 8);
  assert.equal(first.structuredContent.file_path, "README.md");
  assert.equal(
    first.structuredContent.file_bytes,
    Buffer.byteLength(demoFileText),
  );
  const grant = JSON.parse(
    await readFile(first.structuredContent.grant_path, "utf8"),
  );
  assert.equal(grant.version, 2);
  assert.equal(grant.scope, "workspace");
  assert.equal(grant.capability, "remote_embedding");

  const second = await client.callTool({
    name: "zvec_grep_remote_embedding_demo",
    arguments: {
      root,
      text: "second authorized remote query",
      filePath: "README.md",
    },
  });
  assert.equal(second.isError, undefined);
  assert.equal(second.structuredContent.authorization, "existing_workspace");
  assert.equal(elicitations, 1);
  assert.equal(embeddingCalls, 4);

  let crossSessionElicitations = 0;
  const secondClient = await connectClient(
    mcpUrl,
    "remote-embedding-auth-demo-second-session",
    async () => {
      crossSessionElicitations += 1;
      return { action: "accept", content: { decision: "cancel" } };
    },
  );
  t.after(async () => secondClient.close());
  const crossSession = await secondClient.callTool({
    name: "zvec_grep_remote_embedding_demo",
    arguments: {
      root,
      text: "workspace grant survives a new MCP session",
      filePath: "README.md",
    },
  });
  assert.equal(crossSession.isError, undefined);
  assert.equal(
    crossSession.structuredContent.authorization,
    "existing_workspace",
  );
  assert.equal(crossSessionElicitations, 0);
  assert.equal(embeddingCalls, 6);

  const escaped = await client.callTool({
    name: "zvec_grep_remote_embedding_demo",
    arguments: {
      root,
      text: "must not run",
      filePath: "../outside.txt",
    },
  });
  assert.equal(escaped.isError, true);
  assert.match(escaped.content[0].text, /inside the Workspace/);
  assert.equal(embeddingCalls, 6);
});

test("Remote Embedding demo keeps once and session authorization inside zg", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-auth-scope-demo-"),
  );
  const onceRoot = join(temporaryDirectory, "once");
  const sessionRoot = join(temporaryDirectory, "session");
  const cancelRoot = join(temporaryDirectory, "cancel");
  await Promise.all(
    [onceRoot, sessionRoot, cancelRoot].map((root) =>
      mkdir(root, { recursive: true }),
    ),
  );
  await writeFile(join(onceRoot, "README.md"), "Allow once demo.\n");
  await writeFile(join(sessionRoot, "README.md"), "Session grant demo.\n");
  await writeFile(join(cancelRoot, "README.md"), Buffer.from([0, 1, 2, 3]));

  let embeddingCalls = 0;
  const backend = new DaemonBackend({
    version: "1.0.0",
    serviceOptions: {
      authorizationSigningKeyPath: join(temporaryDirectory, "auth.key"),
    },
    modelPoolOptions: {
      createModel: () =>
        new TestEmbeddingModel(async () => {
          embeddingCalls += 1;
        }),
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
  const mcpUrl = new URL(`http://127.0.0.1:${address.port}/mcp`);
  const elicitationCounts = { once: 0, session: 0, cancel: 0 };
  const onElicitation = async (request) => {
    assert.equal(request.params.mode, "form");
    assert.match(request.params.message, /read only after approval/i);
    if (/From\s+once/.test(request.params.message)) {
      elicitationCounts.once += 1;
      return { action: "accept", content: { decision: "allow_once" } };
    }
    if (/From\s+session/.test(request.params.message)) {
      elicitationCounts.session += 1;
      return { action: "accept", content: { decision: "allow_session" } };
    }
    assert.match(request.params.message, /From\s+cancel/);
    elicitationCounts.cancel += 1;
    return { action: "accept", content: { decision: "cancel" } };
  };
  const clients = [];
  const firstClient = await connectClient(
    mcpUrl,
    "remote-embedding-scope-demo-first-session",
    onElicitation,
  );
  clients.push(firstClient);
  t.after(async () => {
    await Promise.all(clients.map((client) => client.close()));
    await server.close();
    await backend.close();
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  for (const text of ["first once call", "second once call"]) {
    const once = await firstClient.callTool({
      name: "zvec_grep_remote_embedding_demo",
      arguments: { root: onceRoot, text, filePath: "README.md" },
    });
    assert.equal(once.isError, undefined);
    assert.equal(once.structuredContent.authorization, "granted_once");
    assert.equal(once.structuredContent.scope, "once");
    assert.equal(once.structuredContent.grant_path, undefined);
  }
  assert.equal(elicitationCounts.once, 2);

  const firstSession = await firstClient.callTool({
    name: "zvec_grep_remote_embedding_demo",
    arguments: {
      root: sessionRoot,
      text: "first session call",
      filePath: "README.md",
    },
  });
  assert.equal(firstSession.structuredContent.authorization, "granted_session");
  assert.equal(firstSession.structuredContent.scope, "session");
  assert.equal(firstSession.structuredContent.grant_path, undefined);

  const reusedSession = await firstClient.callTool({
    name: "zvec_grep_remote_embedding_demo",
    arguments: {
      root: sessionRoot,
      text: "second session call",
      filePath: "README.md",
    },
  });
  assert.equal(
    reusedSession.structuredContent.authorization,
    "existing_session",
  );
  assert.equal(elicitationCounts.session, 1);

  const cancelled = await firstClient.callTool({
    name: "zvec_grep_remote_embedding_demo",
    arguments: {
      root: cancelRoot,
      text: "cancel before reading the binary file",
      filePath: "README.md",
    },
  });
  assert.equal(cancelled.isError, undefined);
  assert.equal(cancelled.structuredContent.authorization, "declined");
  assert.equal(elicitationCounts.cancel, 1);

  const secondClient = await connectClient(
    mcpUrl,
    "remote-embedding-scope-demo-second-session",
    onElicitation,
  );
  clients.push(secondClient);
  const newSession = await secondClient.callTool({
    name: "zvec_grep_remote_embedding_demo",
    arguments: {
      root: sessionRoot,
      text: "new MCP session",
      filePath: "README.md",
    },
  });
  assert.equal(newSession.structuredContent.authorization, "granted_session");
  assert.equal(elicitationCounts.session, 2);
  assert.equal(embeddingCalls, 10);

  await assert.rejects(
    access(
      join(onceRoot, ".zvec-grep", "remote-embedding-authorization.demo.json"),
    ),
  );
  await assert.rejects(
    access(
      join(
        sessionRoot,
        ".zvec-grep",
        "remote-embedding-authorization.demo.json",
      ),
    ),
  );
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
    new URL(`http://127.0.0.1:${address.port}/mcp`),
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
  assert.equal(search.structuredContent.freshness, "fresh");
  assert.ok(search.structuredContent.result.items.length > 0);
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
      "[MODEL_LOAD_FAILED] Server MVP cannot resolve embedding schema for qwen/unsupported-embedding.",
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

async function connectClient(url, name, onElicitation) {
  const client = new Client(
    { name, version: "1.0.0" },
    onElicitation ? { capabilities: { elicitation: { form: {} } } } : undefined,
  );
  if (onElicitation) {
    client.setRequestHandler(ElicitRequestSchema, onElicitation);
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

class TestEmbeddingModel extends EmbeddingModel {
  ref = { provider: "test", model: "deterministic" };
  dimension = 8;
  metric = "cosine";
  supportedContentKinds = ["text"];
  limits = { maxBatchSize: 64 };

  constructor(beforeEmbed = async () => {}) {
    super();
    this.beforeEmbed = beforeEmbed;
  }

  async doEmbed(contents) {
    await this.beforeEmbed();
    return contents.map((content) => {
      const text = content.kind === "text" ? content.text : "";
      const vector = new Array(this.dimension).fill(0);
      for (let index = 0; index < text.length; index++) {
        vector[index % vector.length] += text.charCodeAt(index) / 255;
      }
      const norm =
        Math.sqrt(vector.reduce((sum, value) => sum + value * value, 0)) || 1;
      return vector.map((value) => value / norm);
    });
  }
}
