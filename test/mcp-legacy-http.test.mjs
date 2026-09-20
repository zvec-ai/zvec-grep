import assert from "node:assert/strict";
import test from "node:test";
import { DaemonHttpServer } from "../dist/daemon/http-server.js";

const token = "legacy-http-test-token-at-least-32-characters";
const legacyVersions = [
  "2025-11-25",
  "2025-06-18",
  "2025-03-26",
  "2024-11-05",
  "2024-10-07",
];

test("public MCP endpoint accepts every SDK-supported legacy revision", async (t) => {
  const server = createServer();
  const address = await server.start();
  t.after(async () => server.close());
  const url = new URL(`http://127.0.0.1:${address.port}/mcp`);

  for (const version of legacyVersions) {
    const response = await initialize(url, version);
    assert.equal(response.status, 200);
    assert.equal(response.result.protocolVersion, version);
    assert.ok(response.sessionId);
    const closed = await fetch(url, {
      method: "DELETE",
      headers: headers({ "Mcp-Session-Id": response.sessionId }),
    });
    assert.ok([200, 202].includes(closed.status));
  }
});

test("legacy sessions enforce capacity and expire only after idle TTL", async (t) => {
  const server = createServer({
    legacySessionIdleTtlMs: 25,
    maxLegacySessions: 1,
  });
  const address = await server.start();
  t.after(async () => server.close());
  const url = new URL(`http://127.0.0.1:${address.port}/mcp`);

  const first = await initialize(url, "2025-11-25");
  assert.equal(first.status, 200);
  const full = await initialize(url, "2025-11-25");
  assert.equal(full.status, 503);

  await new Promise((resolve) => setTimeout(resolve, 35));
  const afterExpiry = await initialize(url, "2025-11-25");
  assert.equal(afterExpiry.status, 200);
});

function createServer(options = {}) {
  return new DaemonHttpServer({
    host: "127.0.0.1",
    port: 0,
    token,
    version: "1.0.0",
    backend: {},
    requestStateKey: new Uint8Array(32).fill(3),
    ...options,
  });
}

async function initialize(url, protocolVersion) {
  const response = await fetch(url, {
    method: "POST",
    headers: headers(),
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion,
        capabilities: {},
        clientInfo: { name: "legacy-test", version: "1.0.0" },
      },
    }),
  });
  const text = await response.text();
  const dataLine = text.split("\n").find((line) => line.startsWith("data: "));
  const data = dataLine
    ? JSON.parse(dataLine.slice("data: ".length))
    : JSON.parse(text);
  return {
    status: response.status,
    result: data.result,
    sessionId: response.headers.get("mcp-session-id"),
  };
}

function headers(extra = {}) {
  return {
    Accept: "application/json, text/event-stream",
    Authorization: `Bearer ${token}`,
    "Content-Type": "application/json",
    ...extra,
  };
}
