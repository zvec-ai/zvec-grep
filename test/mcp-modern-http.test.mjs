import assert from "node:assert/strict";
import test from "node:test";
import {
  Client,
  InMemoryTransport,
  StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client";
import { Server } from "@modelcontextprotocol/server";
import { DaemonHttpServer } from "../dist/daemon/http-server.js";
import { registerStdioBridgeElicitationForwarding } from "../dist/mcp/stdio-bridge.js";

const token = "modern-http-test-token-at-least-32-characters";
const root = "/private/tmp/zvec-grep-modern-http";

test("modern HTTP performs MRTR authorization through stdio elicitation forwarding and rejects future versions", async (t) => {
  const target = {
    workspaceRoots: [root],
    workspaceFingerprint: "workspace-fingerprint",
    provider: "qwen",
    model: "text-embedding-v4",
    endpoint: "https://qwen.test/embeddings",
    targetFingerprint: "target-fingerprint",
  };
  let grants = 0;
  let searches = 0;
  const logEvents = [];
  const backend = {
    planSearchAuthorization: async () => ({
      operation: "query",
      target,
      disclosure: { queryText: true, workspaceContent: "none" },
      reason: "query",
      grantPath: `${root}/.zvec-grep/authorization.json`,
    }),
    existingRemoteEmbeddingPermit: async () => undefined,
    grantRemoteEmbedding: async (_plan, scope) => {
      grants += 1;
      return {
        capability: "remote_embedding",
        scope,
        target,
        issuedAt: Date.now(),
        operationId: `operation-${grants}`,
      };
    },
    search: async (input, options) => {
      searches += 1;
      assert.equal(options.authorization.scope, "once");
      return {
        root: input.root,
        freshness: "fresh",
        result: {
          query: "protocol upgrade",
          root: input.root,
          source: "index",
          coverage: "ranked_sample",
          diagnostics: {},
          items: [],
        },
      };
    },
  };
  const server = new DaemonHttpServer({
    host: "127.0.0.1",
    port: 0,
    token,
    version: "1.0.0",
    backend,
    logger: {
      event(name, fields) {
        logEvents.push({ name, fields });
      },
      async flush() {},
    },
    requestStateKey: new Uint8Array(32).fill(5),
  });
  const address = await server.start();
  t.after(async () => server.close());
  const url = new URL(`http://127.0.0.1:${address.port}/mcp/admin`);

  let elicitations = 0;
  const upstream = new Client(
    { name: "modern-mrtr-test", version: "1.0.0" },
    {
      capabilities: { elicitation: { form: {} } },
      versionNegotiation: { mode: { pin: "2026-07-28" } },
    },
  );
  const downstream = new Server(
    { name: "modern-mrtr-downstream-test", version: "1.0.0" },
    { capabilities: {} },
  );
  const qoderLikeHost = new Client(
    { name: "qoder-like-host-test", version: "1.0.0" },
    { capabilities: { elicitation: {} } },
  );
  qoderLikeHost.setRequestHandler("elicitation/create", async () => {
    elicitations += 1;
    return {
      action: "accept",
      content: { decision: "allow_once" },
    };
  });
  registerStdioBridgeElicitationForwarding(upstream, () => downstream);
  const [downstreamTransport, hostTransport] =
    InMemoryTransport.createLinkedPair();
  await Promise.all([
    downstream.connect(downstreamTransport),
    qoderLikeHost.connect(hostTransport),
    upstream.connect(transport(url)),
  ]);
  t.after(async () => {
    await Promise.allSettled([
      upstream.close(),
      downstream.close(),
      qoderLikeHost.close(),
    ]);
  });
  const result = await upstream.callTool({
    name: "zvec_grep_search",
    arguments: { root, query: "protocol upgrade" },
    _meta: {
      traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
    },
  });
  assert.equal(result.isError, undefined);
  assert.equal(elicitations, 1);
  assert.equal(grants, 1);
  assert.equal(searches, 1);
  await waitFor(() =>
    logEvents.some(
      (event) =>
        event.name === "request.completed" &&
        event.fields?.trace_id === "4bf92f3577b34da6a3ce929d0e0e4736",
    ),
  );

  const futureClient = new Client(
    { name: "future-version-test", version: "1.0.0" },
    { versionNegotiation: { mode: { pin: "2099-01-01" } } },
  );
  await assert.rejects(
    futureClient.connect(transport(url)),
    (error) => error?.code === -32022,
  );
  await futureClient.close().catch(() => undefined);
});

function transport(url) {
  return new StreamableHTTPClientTransport(url, {
    requestInit: {
      headers: { Authorization: `Bearer ${token}` },
    },
  });
}

async function waitFor(predicate) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  assert.fail("Expected log event was not emitted.");
}
