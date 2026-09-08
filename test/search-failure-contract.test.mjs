import assert from "node:assert/strict";
import { resolve } from "node:path";
import test from "node:test";
import { Client, InMemoryTransport } from "@modelcontextprotocol/client";
import { EngineError } from "../dist/engine/errors.js";
import { createZvecGrepMcpServer } from "../dist/mcp/tools.js";

const root = resolve("test/fixtures/repository");
const providerFailureCode =
  "ZVEC_GREP.ENGINE.MODELS.QWEN_TEXT_EMBEDDING_V4_API_ERROR";
const errorCodeMetaKey = "io.zvec-grep/search-error-code";
const errorHttpStatusMetaKey = "io.zvec-grep/search-http-status";

async function connect(t, backend, includeSearchStructuredContent) {
  const server = createZvecGrepMcpServer(backend, "1.0.0", {
    toolset: "full",
    includeSearchStructuredContent,
  });
  const client = new Client({
    name: "search-failure-contract-test",
    version: "1.0.0",
  });
  t.after(async () => {
    await client.close();
    await server.close();
  });
  const [clientTransport, serverTransport] =
    InMemoryTransport.createLinkedPair();
  await Promise.all([
    server.connect(serverTransport),
    client.connect(clientTransport),
  ]);
  return client;
}

async function search(client) {
  return await client.callTool({
    name: "zvec_grep_search",
    arguments: { root, query: "database connection reuse" },
  });
}

for (const includeSearchStructuredContent of [false, true]) {
  const mode = includeSearchStructuredContent ? "CLI structured" : "agent text";

  test(`${mode} MCP preserves recoverable search classification without provider details`, async (t) => {
    let searches = 0;
    const client = await connect(
      t,
      {
        search: async () => {
          searches += 1;
          throw new EngineError(
            "Embedding request failed. Bearer contract-message-secret",
            {
              code: providerFailureCode,
              context: "status=503\ncontract-private-source-context",
              cause: new Error("contract-private-provider-response"),
            },
          );
        },
      },
      includeSearchStructuredContent,
    );

    const result = await search(client);
    assert.equal(searches, 1);
    assert.equal(result.isError, true);
    assert.deepEqual(result._meta, {
      [errorCodeMetaKey]: providerFailureCode,
      [errorHttpStatusMetaKey]: 503,
    });
    assert.equal(result.structuredContent, undefined);
    assert.deepEqual(result.content, [
      {
        type: "text",
        text: "Embedding request failed. Bearer [redacted]",
      },
    ]);
    assert.doesNotMatch(
      JSON.stringify(result),
      /contract-message-secret|contract-private-source-context|contract-private-provider-response/,
    );
  });

  test(`${mode} MCP never marks wrapped cancellation as recoverable search failure`, async (t) => {
    let searches = 0;
    const client = await connect(
      t,
      {
        search: async () => {
          searches += 1;
          const cause = new Error("contract-cancellation-cause");
          cause.name = "AbortError";
          throw new EngineError("Embedding request failed.", {
            code: providerFailureCode,
            context: "status=503",
            cause,
          });
        },
      },
      includeSearchStructuredContent,
    );

    const result = await search(client);
    assert.equal(searches, 1);
    assert.equal(result.isError, true);
    assert.equal(result._meta?.[errorCodeMetaKey], undefined);
    assert.equal(result._meta?.[errorHttpStatusMetaKey], undefined);
    assert.equal(result.structuredContent, undefined);
    assert.ok(result.content.some((item) => item.type === "text"));
  });

  test(`${mode} MCP never classifies authorization planning failures as search fallback`, async (t) => {
    let plans = 0;
    let searches = 0;
    const client = await connect(
      t,
      {
        planSearchAuthorization: async () => {
          plans += 1;
          // Even a recoverable-looking provider failure is not recoverable
          // search execution when it occurs before authorization resolves.
          throw new EngineError("Authorization planning failed.", {
            code: providerFailureCode,
            context: "status=503",
          });
        },
        search: async () => {
          searches += 1;
          assert.fail("Search must not run after authorization planning fails");
        },
      },
      includeSearchStructuredContent,
    );

    const result = await search(client);
    assert.equal(plans, 1);
    assert.equal(searches, 0);
    assert.equal(result.isError, true);
    assert.equal(result._meta?.[errorCodeMetaKey], undefined);
    assert.equal(result._meta?.[errorHttpStatusMetaKey], undefined);
    assert.equal(result.structuredContent, undefined);
    assert.ok(result.content.some((item) => item.type === "text"));
  });

  for (const status of [400, 401, 403]) {
    test(`${mode} MCP does not mark provider HTTP ${status} as recoverable`, async (t) => {
      let searches = 0;
      const client = await connect(
        t,
        {
          search: async () => {
            searches += 1;
            throw new EngineError("Embedding provider rejected the request.", {
              code: providerFailureCode,
              context: `status=${status}`,
            });
          },
        },
        includeSearchStructuredContent,
      );

      const result = await search(client);
      assert.equal(searches, 1);
      assert.equal(result.isError, true);
      assert.equal(result._meta?.[errorCodeMetaKey], undefined);
      assert.equal(result._meta?.[errorHttpStatusMetaKey], undefined);
      assert.equal(result.structuredContent, undefined);
      assert.ok(result.content.some((item) => item.type === "text"));
    });
  }
}
