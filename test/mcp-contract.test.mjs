import assert from "node:assert/strict";
import { resolve } from "node:path";
import test from "node:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { ElicitRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { createZvecGrepMcpServer } from "../dist/mcp/tools.js";
import { withProgressHeartbeat } from "../dist/mcp/progress-heartbeat.js";

const root = resolve("test/fixtures/repository");

function createBackend() {
  return {
    index: async (input) => ({
      root: input.root,
      jobId: "job-1",
      state: "queued",
      reused: false,
    }),
    search: async (input) => ({
      root: input.root,
      freshness: "possibly_stale",
      indexing: {
        state: "running",
        completed: 12,
        total: 20,
      },
      result: {
        query:
          input.queries?.join(" ") ??
          input.routes.map((route) => route.query).join(" "),
        root: input.root,
        source: "index",
        coverage: "ranked_sample",
        diagnostics: {},
        items: [
          {
            kind: "indexed_entity",
            rank: 1,
            file: {
              absolutePath: `${input.root}/src/index.ts`,
              relativePath: "src/index.ts",
            },
            range: {
              kind: "text",
              startLine: 1,
              endLine: 2,
              startOffset: 0,
              endOffset: 80,
            },
            content: "x".repeat(100),
            status: "possibly_stale",
            matchedBy: "fts+vector",
          },
        ],
      },
    }),
    indexStatus: async (input) => ({
      root: input.root,
      indexed: true,
      indexPolicy: "enabled",
      source: "index",
      persistent: {
        home: `${input.root}/.zvec-grep`,
        index_path: `${input.root}/.zvec-grep/index`,
        files: { stored: 1, indexed: 1, pending: 0, failed: 0, entities: 1 },
      },
      runtime: {
        watcherActive: true,
        dirtyRevision: 2,
        indexedRevision: 1,
        activeJobId: "job-2",
        jobState: "running",
      },
    }),
    serverStatus: async () => ({
      version: "1.0.0",
      uptimeMs: 100,
      shuttingDown: false,
      activeRuntimes: 1,
      queuedJobs: 0,
      runningJobs: 1,
      models: { loaded: 1, activeLeases: 1 },
    }),
  };
}

async function connect(backend = createBackend()) {
  const server = createZvecGrepMcpServer(backend, "1.0.0");
  const client = new Client({ name: "mcp-contract-test", version: "1.0.0" });
  const [clientTransport, serverTransport] =
    InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    client.connect(clientTransport),
  ]);
  return { client, server };
}

test("server contract exposes the public tools with stable annotations", async (t) => {
  const { client, server } = await connect();
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const listed = await client.listTools();
  const tools = listed.tools.toSorted((left, right) =>
    left.name.localeCompare(right.name),
  );
  assert.deepEqual(
    tools.map((tool) => tool.name),
    [
      "zvec_grep_index",
      "zvec_grep_index_status",
      "zvec_grep_remote_embedding_demo",
      "zvec_grep_search",
      "zvec_grep_server_status",
    ],
  );

  const annotations = Object.fromEntries(
    tools.map((tool) => [tool.name, tool.annotations]),
  );
  assert.equal(annotations.zvec_grep_index.readOnlyHint, false);
  assert.equal(annotations.zvec_grep_remote_embedding_demo.readOnlyHint, false);
  assert.equal(
    annotations.zvec_grep_remote_embedding_demo.idempotentHint,
    false,
  );
  assert.equal(
    annotations.zvec_grep_remote_embedding_demo.openWorldHint,
    false,
  );
  assert.equal(annotations.zvec_grep_search.readOnlyHint, false);
  assert.equal(annotations.zvec_grep_index_status.readOnlyHint, true);
  assert.equal(annotations.zvec_grep_server_status.readOnlyHint, true);
  const search = tools.find((tool) => tool.name === "zvec_grep_search");
  assert.match(search.description, /compact indexing snapshot/);
  assert.match(
    search.outputSchema.properties.indexing.description,
    /possibly stale results/,
  );
  assert.ok(
    search.outputSchema.properties.indexing.properties.state.enum.includes(
      "cancelled",
    ),
  );
  for (const tool of tools) {
    assert.ok(tool.outputSchema, `${tool.name} must declare structured output`);
  }
});

test("index contract documents background submission as the default", async (t) => {
  const { client, server } = await connect();
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const listed = await client.listTools();
  const index = listed.tools.find((tool) => tool.name === "zvec_grep_index");
  assert.ok(index);
  assert.match(
    index.inputSchema.properties.wait.description,
    /Defaults to false/,
  );
  assert.match(
    index.inputSchema.properties.wait.description,
    /zvec_grep_index_status/,
  );
  assert.ok(index.outputSchema.properties.error);
});

test("index result includes an immediately available failure reason", async (t) => {
  const backend = createBackend();
  backend.index = async (input) => ({
    root: input.root,
    jobId: "job-failed",
    state: "failed",
    reused: false,
    error: {
      code: "MODEL_LOAD_FAILED",
      message: "Embedding schema could not be resolved.",
    },
  });
  const { client, server } = await connect(backend);
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const result = await client.callTool({
    name: "zvec_grep_index",
    arguments: { root, embedding: "qwen/unsupported", wait: true },
  });
  assert.equal(result.isError, undefined);
  assert.deepEqual(result.structuredContent.error, {
    code: "MODEL_LOAD_FAILED",
    message: "Embedding schema could not be resolved.",
  });
  assert.match(result.content[0].text, /error_code: MODEL_LOAD_FAILED/);
  assert.match(
    result.content[0].text,
    /Embedding schema could not be resolved/,
  );
});

test("search elicits merged Remote Embedding authorization and reuses session scope", async (t) => {
  const backend = createBackend();
  let elicitations = 0;
  let granted = 0;
  let receivedPermit;
  const target = {
    workspaceRoots: [root],
    workspaceFingerprint: "workspace-fingerprint",
    provider: "qwen",
    model: "text-embedding-v4",
    endpoint: "https://qwen.test/embeddings",
    targetFingerprint: "target-fingerprint",
  };
  backend.planSearchAuthorization = async () => ({
    operation: "query_and_index",
    target,
    disclosure: { queryText: true, workspaceContent: "changed" },
    reason: "query",
    grantPath: `${root}/.zvec-grep/authorization.json`,
  });
  backend.existingRemoteEmbeddingPermit = async () => undefined;
  backend.grantRemoteEmbedding = async (_plan, scope) => {
    granted += 1;
    return {
      capability: "remote_embedding",
      scope,
      target,
      issuedAt: Date.now(),
      operationId: `operation-${granted}`,
    };
  };
  const originalSearch = backend.search;
  backend.search = async (input, options) => {
    receivedPermit = options.authorization;
    return await originalSearch(input);
  };

  const server = createZvecGrepMcpServer(backend, "1.0.0");
  const client = new Client(
    { name: "mcp-auth-test", version: "1.0.0" },
    { capabilities: { elicitation: { form: {} } } },
  );
  client.setRequestHandler(ElicitRequestSchema, async (request) => {
    elicitations += 1;
    assert.match(request.params.message, /Query \+ Index/);
    assert.match(request.params.message, /Changed workspace content/);
    return {
      action: "accept",
      content: { decision: "allow_session" },
    };
  });
  const [clientTransport, serverTransport] =
    InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    client.connect(clientTransport),
  ]);
  t.after(async () => {
    await client.close();
    await server.close();
  });

  for (let index = 0; index < 2; index++) {
    const result = await client.callTool({
      name: "zvec_grep_search",
      arguments: { root, query: "authorization flow" },
    });
    assert.equal(result.isError, undefined);
  }
  assert.equal(elicitations, 1);
  assert.equal(granted, 2);
  assert.equal(receivedPermit.scope, "session");
});

test("Remote Embedding authorization stays alive while waiting for user input", async (t) => {
  const backend = createBackend();
  const target = {
    workspaceRoots: [root],
    workspaceFingerprint: "workspace-fingerprint",
    provider: "qwen",
    model: "text-embedding-v4",
    endpoint: "https://qwen.test/embeddings",
    targetFingerprint: "target-fingerprint",
  };
  backend.planSearchAuthorization = async () => ({
    operation: "query",
    target,
    disclosure: { queryText: true, workspaceContent: "none" },
    reason: "query",
    grantPath: `${root}/.zvec-grep/authorization.json`,
  });
  backend.existingRemoteEmbeddingPermit = async () => undefined;
  backend.grantRemoteEmbedding = async (_plan, scope) => ({
    capability: "remote_embedding",
    scope,
    target,
    issuedAt: Date.now(),
    operationId: "long-wait-operation",
  });

  const server = createZvecGrepMcpServer(backend, "1.0.0", {
    authorizationHeartbeatMs: 5,
    authorizationRequestTimeoutMs: 20,
  });
  const client = new Client(
    { name: "mcp-long-auth-test", version: "1.0.0" },
    { capabilities: { elicitation: { form: {} } } },
  );
  client.setRequestHandler(
    ElicitRequestSchema,
    async (_request, extra) =>
      await withProgressHeartbeat(
        extra,
        async () => {
          await new Promise((resolve) => setTimeout(resolve, 80));
          return {
            action: "accept",
            content: { decision: "allow_once" },
          };
        },
        {
          intervalMs: 5,
          message: "Waiting for simulated user input.",
        },
      ),
  );
  const [clientTransport, serverTransport] =
    InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    client.connect(clientTransport),
  ]);
  t.after(async () => {
    await client.close();
    await server.close();
  });

  let progressNotifications = 0;
  const result = await client.callTool(
    {
      name: "zvec_grep_search",
      arguments: { root, query: "authorization flow" },
    },
    undefined,
    {
      timeout: 20,
      onprogress: () => {
        progressNotifications += 1;
      },
      resetTimeoutOnProgress: true,
    },
  );

  assert.equal(result.isError, undefined);
  assert.ok(progressNotifications >= 2);
});

test("root tools require an absolute root", async (t) => {
  const { client, server } = await connect();
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const missing = await client.callTool({
    name: "zvec_grep_index",
    arguments: {},
  });
  assert.equal(missing.isError, true);
  assert.match(missing.content[0].text, /root/i);

  const statusRelative = await client.callTool({
    name: "zvec_grep_index_status",
    arguments: { root: "relative/path" },
  });
  assert.equal(statusRelative.isError, true);
  assert.match(statusRelative.content[0].text, /absolute path/i);

  const searchRelative = await client.callTool({
    name: "zvec_grep_search",
    arguments: { root: "relative/path", query: "query" },
  });
  assert.equal(searchRelative.isError, true);
  assert.match(searchRelative.content[0].text, /absolute path/i);
});

test("search normalizes query, path and time inputs before calling the backend", async (t) => {
  let received;
  const backend = createBackend();
  backend.search = async (input) => {
    received = input;
    return createBackend().search(input);
  };
  const { client, server } = await connect(backend);
  t.after(async () => {
    await client.close();
    await server.close();
  });

  await client.callTool({
    name: "zvec_grep_search",
    arguments: {
      root,
      query: "  service lifecycle  ",
      fts: ["  ModelLease  "],
      include: "src/**, test/**",
      exclude: ["dist/**", " coverage/** "],
      globs: ["!*.ts", "keep.ts"],
      insensitiveGlobs: ["README*"],
      fileTypes: ["ts"],
      excludedFileTypes: ["json"],
      fuse: true,
      hidden: true,
      noIgnore: true,
      ignoreFiles: [".ignore-extra"],
      maxDepth: 3,
      maxFileSizeBytes: 4096,
      follow: true,
      embeddingConcurrency: 2,
      modifiedAfter: "2025-01-01T00:00:00.000Z",
    },
  });

  assert.deepEqual(received.queries, ["service lifecycle"]);
  assert.deepEqual(received.routes, [{ mode: "fts", query: "ModelLease" }]);
  assert.deepEqual(received.includePaths, ["src/**", "test/**"]);
  assert.deepEqual(received.excludePaths, ["dist/**", "coverage/**"]);
  assert.deepEqual(received.globs, ["!*.ts", "keep.ts"]);
  assert.deepEqual(received.insensitiveGlobs, ["README*"]);
  assert.deepEqual(received.fileTypes, ["ts"]);
  assert.deepEqual(received.excludedFileTypes, ["json"]);
  assert.equal(received.fuse, true);
  assert.equal(received.hidden, true);
  assert.equal(received.noIgnore, true);
  assert.deepEqual(received.ignoreFiles, [".ignore-extra"]);
  assert.equal(received.maxDepth, 3);
  assert.equal(received.maxFileSizeBytes, 4096);
  assert.equal(received.follow, true);
  assert.equal(received.embeddingConcurrency, 2);
  assert.equal(received.modifiedAfter, Date.parse("2025-01-01T00:00:00.000Z"));
  assert.equal(received.freshness, "eventual");
  assert.equal(received.autoUpdate, true);
});

test("search can return the current index without scheduling an update", async (t) => {
  let received;
  const backend = createBackend();
  backend.search = async (input) => {
    received = input;
    return createBackend().search(input);
  };
  const { client, server } = await connect(backend);
  t.after(async () => {
    await client.close();
    await server.close();
  });

  await client.callTool({
    name: "zvec_grep_search",
    arguments: { root, query: "query", autoUpdate: false },
  });

  assert.equal(received.freshness, "eventual");
  assert.equal(received.autoUpdate, false);
});

test("input upper bounds are enforced", async (t) => {
  const { client, server } = await connect();
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const excessiveLimit = await client.callTool({
    name: "zvec_grep_search",
    arguments: { root, query: "query", limit: 51 },
  });
  assert.equal(excessiveLimit.isError, true);
  assert.match(excessiveLimit.content[0].text, /less than or equal to 50/i);

  const excessiveContent = await client.callTool({
    name: "zvec_grep_search",
    arguments: { root, query: "query", maxContentChars: 6001 },
  });
  assert.equal(excessiveContent.isError, true);
  assert.match(excessiveContent.content[0].text, /less than or equal to 6000/i);

  const excessiveQuery = await client.callTool({
    name: "zvec_grep_search",
    arguments: { root, query: "q".repeat(4_001) },
  });
  assert.equal(excessiveQuery.isError, true);

  const excessiveQueryGroups = await client.callTool({
    name: "zvec_grep_search",
    arguments: {
      root,
      queries: Array.from({ length: 33 }, (_, index) => `query-${index}`),
    },
  });
  assert.equal(excessiveQueryGroups.isError, true);

  const excessivePath = await client.callTool({
    name: "zvec_grep_search",
    arguments: { root, query: "query", include: "p".repeat(1_025) },
  });
  assert.equal(excessivePath.isError, true);

  const excessivePathFilters = await client.callTool({
    name: "zvec_grep_search",
    arguments: {
      root,
      query: "query",
      exclude: Array.from({ length: 129 }, (_, index) => `path-${index}/**`),
    },
  });
  assert.equal(excessivePathFilters.isError, true);
});

test("all tools return output-schema-compatible structured content", async (t) => {
  const { client, server } = await connect();
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const calls = [
    ["zvec_grep_index", { root }],
    ["zvec_grep_search", { root, query: "query", maxContentChars: 20 }],
    ["zvec_grep_index_status", { root }],
    ["zvec_grep_server_status", {}],
  ];
  for (const [name, arguments_] of calls) {
    const result = await client.callTool({ name, arguments: arguments_ });
    assert.equal(result.isError, undefined, `${name} returned an error`);
    assert.ok(result.structuredContent, `${name} omitted structured content`);
  }

  const status = await client.callTool({
    name: "zvec_grep_index_status",
    arguments: { root },
  });
  assert.equal(status.structuredContent.persistent.home, `${root}/.zvec-grep`);
  assert.equal(status.structuredContent.persistent.files.indexed, 1);

  const search = await client.callTool({
    name: "zvec_grep_search",
    arguments: { root, query: "query", maxContentChars: 20 },
  });
  assert.match(
    search.structuredContent.result.items[0].content,
    /truncated 80 chars/,
  );
  assert.deepEqual(search.structuredContent.indexing, {
    state: "running",
    completed: 12,
    total: 20,
  });
  assert.equal(search.structuredContent.update_job_id, undefined);
  assert.match(search.content[0].text, /indexing: running \(12\/20\)/);
});
