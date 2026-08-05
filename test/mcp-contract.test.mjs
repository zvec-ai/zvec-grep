import assert from "node:assert/strict";
import { resolve } from "node:path";
import test from "node:test";
import { Client, InMemoryTransport } from "@modelcontextprotocol/client";
import {
  createZvecGrepMcpServer,
  ZVEC_GREP_AGENT_MCP_INSTRUCTIONS,
  ZVEC_GREP_FULL_MCP_INSTRUCTIONS,
  ZVEC_GREP_MCP_INSTRUCTIONS,
} from "../dist/mcp/tools.js";
import {
  DEFAULT_MCP_TOOLSET,
  parseMcpToolset,
  resolveMcpToolset,
} from "../dist/mcp/toolset.js";
import { indexProgressFromMessage } from "../dist/index-progress.js";
import { formatAgentContextResult } from "../dist/cli/format/context.js";

const root = resolve("test/fixtures/repository");
const longIndexedContent = "x".repeat(8_000);
const longRgContent = "r".repeat(8_000);

function createBackend() {
  return {
    index: async (input) => ({
      root: input.root,
      jobId: input.drop ? "drop" : "job-1",
      state: input.drop ? "succeeded" : "queued",
      reused: false,
      action: input.drop ? "drop" : "index",
      dropped: input.drop ? true : undefined,
    }),
    dropIndex: async (input) => ({
      root: input.root,
      removed: true,
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
            content: longIndexedContent,
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
        index_path: `${input.root}/.zvec-grep/index.zvec`,
        files: {
          stored: 1,
          indexed: 1,
          pending: 0,
          failed: 0,
          added: 1,
          modified: 2,
          deleted: 3,
          unchanged: 0,
          entities: 1,
          truncated_fragments: 2,
        },
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
    rg: async (input) => ({
      root: input.root,
      result: {
        query: input.pattern ?? input.patterns?.[0] ?? "",
        root: input.root,
        source: "rg",
        coverage: "rg_exhaustive",
        diagnostics: {},
        items: [
          {
            kind: "lexical_match",
            rank: 1,
            file: {
              absolutePath: `${input.root}/src/index.ts`,
              relativePath: "src/index.ts",
            },
            range: {
              kind: "text",
              startLine: 1,
              endLine: 1,
              startOffset: 0,
              endOffset: 8,
            },
            content: longRgContent,
            status: "fresh",
            matchedBy: "lexical",
          },
        ],
      },
    }),
  };
}

async function connect(backend = createBackend(), options = {}) {
  const server = createZvecGrepMcpServer(backend, "1.0.0", options);
  const client = new Client({ name: "mcp-contract-test", version: "1.0.0" });
  const [clientTransport, serverTransport] =
    InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    client.connect(clientTransport),
  ]);
  return { client, server };
}

async function connectFull(backend = createBackend()) {
  return await connect(backend, { toolset: "full" });
}

test("MCP toolset resolution prefers explicit configuration and defaults to agent", () => {
  assert.equal(DEFAULT_MCP_TOOLSET, "agent");
  assert.equal(resolveMcpToolset(), "agent");
  assert.equal(resolveMcpToolset(undefined, "full"), "full");
  assert.equal(resolveMcpToolset("agent", "full"), "agent");
  assert.equal(parseMcpToolset("full"), "full");
  assert.throws(() => parseMcpToolset("all"), /Expected "agent" or "full"/);
});

test("default agent contract exposes only indexed search", async (t) => {
  const backend = createBackend();
  let managementCalls = 0;
  backend.index = async (input) => {
    managementCalls += 1;
    return await createBackend().index(input);
  };
  backend.dropIndex = async (input) => {
    managementCalls += 1;
    return await createBackend().dropIndex(input);
  };
  backend.indexStatus = async (input) => {
    managementCalls += 1;
    return await createBackend().indexStatus(input);
  };
  backend.serverStatus = async () => {
    managementCalls += 1;
    return await createBackend().serverStatus();
  };
  const { client, server } = await connect(backend);
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const listed = await client.listTools();
  assert.deepEqual(listed.tools.map((tool) => tool.name).toSorted(), [
    "zvec_grep_search",
  ]);

  const instructions = client.getInstructions();
  assert.equal(instructions, ZVEC_GREP_AGENT_MCP_INSTRUCTIONS);
  assert.equal(instructions, ZVEC_GREP_MCP_INSTRUCTIONS);
  assert.doesNotMatch(
    instructions,
    /zvec_grep_(?:rg|index|index_drop|index_status|server_status)/,
  );
  const search = listed.tools.find((tool) => tool.name === "zvec_grep_search");
  assert.ok(search);
  assert.match(search.description, /answer requires architecture, lifecycle/);
  assert.match(search.description, /mixed tasks/);
  assert.match(search.description, /native Grep or rg for focused follow-up/);
  assert.doesNotMatch(search.description, /index first/);
  assert.doesNotMatch(
    search.description,
    /zvec_grep_(?:rg|index|index_drop|index_status|server_status)/,
  );

  for (const [name, arguments_] of [
    ["zvec_grep_rg", { root, command: "rg query" }],
    ["zvec_grep_index", { root }],
    ["zvec_grep_index_drop", { root }],
    ["zvec_grep_index_status", { root }],
    ["zvec_grep_server_status", {}],
  ]) {
    await assert.rejects(
      client.callTool({ name, arguments: arguments_ }),
      (error) => error?.code === -32602 && /not found/i.test(error.message),
    );
  }
  assert.equal(managementCalls, 0);
});

test("full server contract exposes all tools with stable annotations", async (t) => {
  const { client, server } = await connectFull();
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
      "zvec_grep_index_drop",
      "zvec_grep_index_status",
      "zvec_grep_rg",
      "zvec_grep_search",
      "zvec_grep_server_status",
    ],
  );
  const instructions = client.getInstructions();
  assert.equal(instructions, ZVEC_GREP_FULL_MCP_INSTRUCTIONS);
  assert.notEqual(instructions, ZVEC_GREP_AGENT_MCP_INSTRUCTIONS);
  const toolContracts = JSON.stringify(
    tools.map((tool) => ({
      title: tool.title,
      description: tool.description,
      inputSchema: tool.inputSchema,
      outputSchema: tool.outputSchema,
    })),
  );
  assert.doesNotMatch(toolContracts, /\bCLI\b/i);
  assert.doesNotMatch(toolContracts, /`?zg(?:\s|`)/i);
  assert.match(instructions, /scope of the requested answer/);
  assert.match(instructions, /Use zvec_grep_rg first only when/);
  assert.match(instructions, /Use zvec_grep_search first when/);
  assert.match(instructions, /treat it as a mixed task/);
  assert.match(instructions, /make at least one appropriate zvec-grep call/);
  assert.match(instructions, /Do not launch a sub-agent solely to locate code/);
  assert.match(instructions, /repository search, status, indexing, deletion/);
  assert.match(instructions, /zvec_grep_index_status/);
  assert.match(instructions, /zvec_grep_server_status/);
  assert.doesNotMatch(instructions, /\bCLI\b/i);
  assert.doesNotMatch(instructions, /`?zg(?:\s|`)/i);

  const annotations = Object.fromEntries(
    tools.map((tool) => [tool.name, tool.annotations]),
  );
  assert.equal(annotations.zvec_grep_index.readOnlyHint, false);
  assert.equal(annotations.zvec_grep_index.destructiveHint, true);
  assert.equal(annotations.zvec_grep_rg.readOnlyHint, true);
  assert.equal(annotations.zvec_grep_index_drop.readOnlyHint, false);
  assert.equal(annotations.zvec_grep_index_drop.destructiveHint, true);
  assert.equal(annotations.zvec_grep_index_drop.idempotentHint, true);
  assert.equal(annotations.zvec_grep_search.readOnlyHint, false);
  assert.equal(annotations.zvec_grep_index_status.readOnlyHint, true);
  assert.equal(annotations.zvec_grep_server_status.readOnlyHint, true);
  const index = tools.find((tool) => tool.name === "zvec_grep_index");
  assert.match(index.title, /Ensure or drop/);
  assert.match(index.description, /Do not call this tool/);
  assert.match(index.description, /index deletion/);
  const search = tools.find((tool) => tool.name === "zvec_grep_search");
  assert.match(search.description, /answer requires architecture, lifecycle/);
  assert.match(search.description, /mixed tasks/);
  assert.match(search.description, /managed ripgrep for focused follow-up/);
  assert.doesNotMatch(search.description, /index first/);
  assert.match(search.description, /missing indexes/);
  for (const tool of [index, search]) {
    assert.match(
      tool.inputSchema.properties.fileTypes.description,
      /rg --type-list/,
    );
    assert.match(
      tool.inputSchema.properties.excludedFileTypes.description,
      /rg --type-list/,
    );
    assert.doesNotMatch(
      tool.inputSchema.properties.fileTypes.description,
      /extension aliases/i,
    );
    assert.doesNotMatch(
      tool.inputSchema.properties.excludedFileTypes.description,
      /extension aliases/i,
    );
  }
  const rg = tools.find((tool) => tool.name === "zvec_grep_rg");
  assert.match(rg.description, /exhaustive, AST-enriched managed ripgrep/);
  assert.match(
    rg.description,
    /Use it first only when the answer can be obtained/,
  );
  assert.match(rg.description, /exhaustive by default/);
  assert.match(rg.description, /append `\| head -N` only/);
  assert.deepEqual(Object.keys(rg.inputSchema.properties).toSorted(), [
    "command",
    "root",
  ]);
  assert.match(
    rg.inputSchema.properties.command.description,
    /rg command you would otherwise run/i,
  );
  assert.match(
    rg.inputSchema.properties.command.description,
    /exhaustive by default/i,
  );
  assert.equal(rg.outputSchema, undefined);
  assert.equal(search.outputSchema, undefined);
  for (const tool of tools.filter(
    (tool) => tool.name !== "zvec_grep_rg" && tool.name !== "zvec_grep_search",
  )) {
    assert.ok(tool.outputSchema, `${tool.name} must declare structured output`);
  }
});

test("index contract documents background submission as the default", async (t) => {
  const { client, server } = await connectFull();
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
  assert.match(index.inputSchema.properties.drop.description, /index deletion/);
  assert.match(
    index.inputSchema.properties.embedding.description,
    /never guess/,
  );
  assert.match(index.inputSchema.properties.rebuild.description, /requested/);
  assert.ok(index.outputSchema.properties.error);
});

test("index drop returns the daemon deletion result", async (t) => {
  const { client, server } = await connectFull();
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const result = await client.callTool({
    name: "zvec_grep_index_drop",
    arguments: { root },
  });
  assert.equal(result.isError, undefined);
  assert.deepEqual(result.structuredContent, { root, removed: true });
  assert.match(result.content[0].text, /Dropped Workspace index/);
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
  const { client, server } = await connectFull(backend);
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

test("index streams daemon progress through MCP", async (t) => {
  const backend = createBackend();
  backend.index = async (input, options) => {
    options.onProgress({
      phase: "scanning",
      detail: "Scanning workspace...",
    });
    options.onProgress({
      phase: "indexing",
      filesIndexed: 2,
      filesTotal: 5,
      detail: "embedding src/example.ts",
    });
    options.onProgress({ phase: "done", detail: "Indexing complete" });
    return {
      root: input.root,
      jobId: "job-progress",
      state: "succeeded",
      reused: false,
    };
  };
  const { client, server } = await connectFull(backend);
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const progressLines = [];
  const progressUpdates = [];
  const result = await client.callTool(
    {
      name: "zvec_grep_index",
      arguments: { root, embedding: "test/deterministic", wait: true },
    },
    {
      onprogress: (progress) => {
        const update = indexProgressFromMessage(progress.message);
        if (update) {
          progressLines.push(update.line);
          progressUpdates.push(update.progress);
        }
      },
    },
  );

  assert.equal(result.structuredContent.state, "succeeded");
  assert.deepEqual(progressLines, [
    "Scanning workspace...",
    "Indexing files: 2/5 embedding src/example.ts",
    "Indexing complete",
  ]);
  assert.deepEqual(progressUpdates[1], {
    phase: "indexing",
    filesIndexed: 2,
    filesTotal: 5,
    detail: "embedding src/example.ts",
  });
});

test("index authorization keeps the requested model and offers no local fallback", async (t) => {
  const backend = createBackend();
  let receivedInput;
  let receivedPermit;
  const target = {
    workspaceRoots: [root],
    workspaceFingerprint: "workspace-fingerprint",
    provider: "qwen",
    model: "text-embedding-v4",
    endpoint: "https://qwen.test/embeddings",
    targetFingerprint: "target-fingerprint",
  };
  backend.planIndexAuthorization = async () => ({
    operation: "query_and_index",
    target,
    disclosure: { queryText: false, workspaceContent: "selected" },
    reason: "index_create",
    grantPath: `${root}/.zvec-grep/authorization.json`,
  });
  backend.existingRemoteEmbeddingPermit = async () => undefined;
  backend.grantRemoteEmbedding = async (_plan, scope) => ({
    capability: "remote_embedding",
    scope,
    target,
    issuedAt: Date.now(),
    operationId: "index-operation",
  });
  const originalIndex = backend.index;
  backend.index = async (input, options) => {
    receivedInput = input;
    receivedPermit = options.authorization;
    return await originalIndex(input);
  };

  const server = createZvecGrepMcpServer(backend, "1.0.0", {
    toolset: "full",
  });
  const client = new Client(
    { name: "mcp-index-auth-test", version: "1.0.0" },
    { capabilities: { elicitation: { form: {} } } },
  );
  client.setRequestHandler("elicitation/create", async (request) => {
    const decisions = request.params.requestedSchema.properties.decision.oneOf;
    assert.deepEqual(
      decisions.map((decision) => decision.const),
      ["allow_once", "allow_workspace", "cancel"],
    );
    assert.doesNotMatch(JSON.stringify(decisions), /local/i);
    return {
      action: "accept",
      content: { decision: "allow_once" },
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

  const result = await client.callTool({
    name: "zvec_grep_index",
    arguments: { root, embedding: "qwen/text-embedding-v4" },
  });

  assert.equal(result.isError, undefined);
  assert.equal(receivedInput.embedding, "qwen/text-embedding-v4");
  assert.equal(receivedPermit.scope, "once");
});

test("search elicits merged Remote Embedding authorization for every once-scoped operation", async (t) => {
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
  client.setRequestHandler("elicitation/create", async (request) => {
    elicitations += 1;
    assert.equal(
      request.params.message,
      [
        "Remote Embedding authorization",
        "",
        "Send query text and changed workspace files?",
        "",
        "  From  repository",
        "  To    qwen/text-embedding-v4",
        "        qwen.test",
        "",
        "API charges may apply.",
      ].join("\n"),
    );
    assert.equal(request.params.message.includes("\n"), true);
    assert.equal(request.params.message.includes(root), false);
    assert.equal(
      request.params.requestedSchema.properties.decision.description,
      "Allow this operation once or remember permission for this workspace.",
    );
    return {
      action: "accept",
      content: { decision: "allow_once" },
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
  assert.equal(elicitations, 2);
  assert.equal(granted, 2);
  assert.equal(receivedPermit.scope, "once");
});

test("Remote Embedding authorization completes through a delayed MRTR response", async (t) => {
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

  const server = createZvecGrepMcpServer(backend, "1.0.0");
  const client = new Client(
    { name: "mcp-long-auth-test", version: "1.0.0" },
    { capabilities: { elicitation: { form: {} } } },
  );
  client.setRequestHandler("elicitation/create", async () => {
    await new Promise((resolve) => setTimeout(resolve, 80));
    return {
      action: "accept",
      content: { decision: "allow_once" },
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

  const result = await client.callTool(
    {
      name: "zvec_grep_search",
      arguments: { root, query: "authorization flow" },
    },
    {
      timeout: 500,
    },
  );

  assert.equal(result.isError, undefined);
});

test("root tools require an absolute root", async (t) => {
  const { client, server } = await connectFull();
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

  const dropRelative = await client.callTool({
    name: "zvec_grep_index_drop",
    arguments: { root: "relative/path" },
  });
  assert.equal(dropRelative.isError, true);
  assert.match(dropRelative.content[0].text, /absolute path/i);

  const searchRelative = await client.callTool({
    name: "zvec_grep_search",
    arguments: { root: "relative/path", query: "query" },
  });
  assert.equal(searchRelative.isError, true);
  assert.match(searchRelative.content[0].text, /absolute path/i);

  const rgRelative = await client.callTool({
    name: "zvec_grep_rg",
    arguments: { root: "relative/path", command: "rg query" },
  });
  assert.equal(rgRelative.isError, true);
  assert.match(rgRelative.content[0].text, /absolute path/i);
});

test("index supports drop mode", async (t) => {
  let received;
  const backend = createBackend();
  backend.index = async (input) => {
    received = input;
    return createBackend().index(input);
  };
  const { client, server } = await connectFull(backend);
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const dropped = await client.callTool({
    name: "zvec_grep_index",
    arguments: { root, drop: true },
  });

  assert.equal(received.drop, true);
  assert.equal(dropped.structuredContent.action, "drop");
  assert.equal(dropped.structuredContent.dropped, true);
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

test("full toolset rg forwards a managed ripgrep command before calling the backend", async (t) => {
  let received;
  const backend = createBackend();
  backend.rg = async (input) => {
    received = input;
    return createBackend().rg(input);
  };
  const { client, server } = await connectFull(backend);
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const result = await client.callTool({
    name: "zvec_grep_rg",
    arguments: {
      root,
      command: "rg -Fi -C 2 -g 'src/**' -g '!dist/**' needle src test",
    },
  });

  assert.equal(
    received.command,
    "rg -Fi -C 2 -g 'src/**' -g '!dist/**' needle src test",
  );
  assert.equal(result.structuredContent, undefined);
  const expected = formatAgentContextResult(
    (await createBackend().rg({ root, command: "rg needle" })).result,
    {},
  );
  assert.equal(result.content[0].text, expected);
  assert.match(result.content[0].text, /^src\/index\.ts\n {2}1:\t/);
  assert.doesNotMatch(result.content[0].text, /rank=|matchedBy=|source:/);
});

test("full toolset rg reports truncation only for an explicit output bound", async (t) => {
  const backend = createBackend();
  backend.rg = async (input) => {
    const response = await createBackend().rg(input);
    response.result.coverage = "rg_truncated";
    return response;
  };
  const { client, server } = await connectFull(backend);
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const result = await client.callTool({
    name: "zvec_grep_rg",
    arguments: {
      root,
      command: "rg needle | head -1",
    },
  });

  assert.match(result.content[0].text, /explicit output bound/);
  assert.match(result.content[0].text, /Remove or increase.*`head`/);
  assert.doesNotMatch(result.content[0].text, /inspect.*before searching/i);
});

test("full toolset rg command input is required and bounded", async (t) => {
  const { client, server } = await connectFull();
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const excessive = await client.callTool({
    name: "zvec_grep_rg",
    arguments: { root, command: `rg ${"n".repeat(4_001)}` },
  });
  assert.equal(excessive.isError, true);

  const missing = await client.callTool({
    name: "zvec_grep_rg",
    arguments: { root },
  });
  assert.equal(missing.isError, true);
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
  assert.match(
    excessiveLimit.content[0].text,
    /less than or equal to 50|expected number to be <=50/i,
  );

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
    arguments: { root, query: "query", globs: "p".repeat(1_025) },
  });
  assert.equal(excessivePath.isError, true);

  const excessivePathFilters = await client.callTool({
    name: "zvec_grep_search",
    arguments: {
      root,
      query: "query",
      globs: Array.from({ length: 129 }, (_, index) => `path-${index}/**`),
    },
  });
  assert.equal(excessivePathFilters.isError, true);
});

test("structured tools return schema-compatible content and searches return only compact text", async (t) => {
  const { client, server } = await connectFull();
  t.after(async () => {
    await client.close();
    await server.close();
  });

  const calls = [
    ["zvec_grep_index", { root }],
    ["zvec_grep_index_drop", { root }],
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
  assert.equal(
    status.structuredContent.persistent.files.truncated_fragments,
    2,
  );
  assert.deepEqual(
    {
      added: status.structuredContent.persistent.files.added,
      modified: status.structuredContent.persistent.files.modified,
      deleted: status.structuredContent.persistent.files.deleted,
      unchanged: status.structuredContent.persistent.files.unchanged,
    },
    { added: 1, modified: 2, deleted: 3, unchanged: 0 },
  );

  const search = await client.callTool({
    name: "zvec_grep_search",
    arguments: { root, query: "query" },
  });
  assert.equal(search.structuredContent, undefined);
  assert.match(search.content[0].text, /^freshness: possibly_stale$/m);
  assert.match(search.content[0].text, /indexing: running \(12\/20\)/);
  assert.match(search.content[0].text, /src\/index\.ts:1-2/);
  assert.equal(search.content[0].text.includes(longIndexedContent), false);

  const rg = await client.callTool({
    name: "zvec_grep_rg",
    arguments: { root, command: "rg needle" },
  });
  assert.equal(rg.structuredContent, undefined);
  assert.equal(rg.content[0].text.includes(longRgContent), true);
  assert.equal(
    rg.content[0].text,
    formatAgentContextResult(
      (await createBackend().rg({ root, command: "rg needle" })).result,
      {},
    ),
  );
});
