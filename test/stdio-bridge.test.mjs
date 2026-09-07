import assert from "node:assert/strict";
import test from "node:test";
import { Client, InMemoryTransport } from "@modelcontextprotocol/client";
import { Server } from "@modelcontextprotocol/server";
import { REMOTE_EMBEDDING_ELICITATION_UNSUPPORTED_MESSAGE } from "../dist/authorization/prompt.js";
import {
  createStdioBridgeStopCheck,
  registerStdioBridgeElicitationForwarding,
  shouldStopStdioBridge,
} from "../dist/mcp/stdio-bridge.js";

const connectedDaemon = {
  running: true,
  ready: true,
  pid: 1234,
  serverUrl: "http://127.0.0.1:7999/mcp",
  mcpToolset: "agent",
};

test("stdio monitor requires consecutive missing statuses and resets on recovery", () => {
  const check = createStdioBridgeStopCheck(connectedDaemon);
  const missing = { running: false, ready: false };
  assert.equal(check(missing), false);
  assert.equal(check(missing), false);
  assert.equal(check({ ...connectedDaemon, ready: false }), false);
  assert.equal(check(missing), false);
  assert.equal(check(missing), false);
  assert.equal(check(missing), true);
});

test("stdio monitor stops immediately on a confirmed identity change", () => {
  for (const changed of [
    { ...connectedDaemon, pid: 5678 },
    { ...connectedDaemon, serverUrl: "http://127.0.0.1:8000/mcp" },
  ]) {
    assert.equal(createStdioBridgeStopCheck(connectedDaemon)(changed), true);
  }
});

test("stdio bridge tolerates a transient health-check timeout", () => {
  assert.equal(
    shouldStopStdioBridge(connectedDaemon, {
      ...connectedDaemon,
      ready: false,
    }),
    false,
  );
});

test("stdio bridge stops when the daemon identity changes", () => {
  assert.equal(
    shouldStopStdioBridge(connectedDaemon, {
      running: false,
      ready: false,
    }),
    true,
  );
  assert.equal(
    shouldStopStdioBridge(connectedDaemon, {
      ...connectedDaemon,
      pid: 5678,
    }),
    true,
  );
  assert.equal(
    shouldStopStdioBridge(connectedDaemon, {
      ...connectedDaemon,
      serverUrl: "http://127.0.0.1:8000/mcp",
    }),
    true,
  );
});

test("stdio bridge forwards elicitation requests to the downstream host", async (t) => {
  const daemon = new Server(
    { name: "stdio-bridge-daemon-test", version: "1.0.0" },
    { capabilities: {} },
  );
  const upstream = new Client(
    { name: "stdio-bridge-upstream-test", version: "1.0.0" },
    { capabilities: { elicitation: { form: {} } } },
  );
  const downstream = new Server(
    { name: "stdio-bridge-downstream-test", version: "1.0.0" },
    { capabilities: {} },
  );
  const host = new Client(
    { name: "stdio-bridge-host-test", version: "1.0.0" },
    { capabilities: { elicitation: {} } },
  );
  let receivedRequest;
  host.setRequestHandler("elicitation/create", async (request) => {
    receivedRequest = request;
    return {
      action: "accept",
      content: { decision: "allow_once" },
    };
  });
  registerStdioBridgeElicitationForwarding(upstream, () => downstream);

  const [daemonTransport, upstreamTransport] =
    InMemoryTransport.createLinkedPair();
  const [downstreamTransport, hostTransport] =
    InMemoryTransport.createLinkedPair();
  await Promise.all([
    daemon.connect(daemonTransport),
    upstream.connect(upstreamTransport),
    downstream.connect(downstreamTransport),
    host.connect(hostTransport),
  ]);
  t.after(async () => {
    await Promise.allSettled([
      daemon.close(),
      upstream.close(),
      downstream.close(),
      host.close(),
    ]);
  });

  const result = await daemon.request({
    method: "elicitation/create",
    params: {
      mode: "form",
      message: "Authorize Remote Embedding?",
      requestedSchema: {
        type: "object",
        properties: {
          decision: {
            type: "string",
            enum: ["allow_once", "cancel"],
          },
        },
        required: ["decision"],
      },
    },
  });

  assert.equal(receivedRequest.params.message, "Authorize Remote Embedding?");
  assert.deepEqual(result, {
    action: "accept",
    content: { decision: "allow_once" },
  });
});

test("stdio bridge distinguishes a missing elicitation handler from other host errors", async (t) => {
  const daemon = new Server(
    { name: "stdio-bridge-daemon-test", version: "1.0.0" },
    { capabilities: {} },
  );
  const upstream = new Client(
    { name: "stdio-bridge-upstream-test", version: "1.0.0" },
    { capabilities: { elicitation: { form: {} } } },
  );
  const downstream = new Server(
    { name: "stdio-bridge-downstream-test", version: "1.0.0" },
    { capabilities: {} },
  );
  const host = new Client(
    { name: "stdio-bridge-host-test", version: "1.0.0" },
    { capabilities: { elicitation: {} } },
  );
  registerStdioBridgeElicitationForwarding(upstream, () => downstream);

  const [daemonTransport, upstreamTransport] =
    InMemoryTransport.createLinkedPair();
  const [downstreamTransport, hostTransport] =
    InMemoryTransport.createLinkedPair();
  await Promise.all([
    daemon.connect(daemonTransport),
    upstream.connect(upstreamTransport),
    downstream.connect(downstreamTransport),
    host.connect(hostTransport),
  ]);
  t.after(async () => {
    await Promise.allSettled([
      daemon.close(),
      upstream.close(),
      downstream.close(),
      host.close(),
    ]);
  });

  const request = {
    method: "elicitation/create",
    params: {
      mode: "form",
      message: "Authorize Remote Embedding?",
      requestedSchema: {
        type: "object",
        properties: {},
      },
    },
  };
  await assert.rejects(daemon.request(request), {
    code: -32603,
    message: REMOTE_EMBEDDING_ELICITATION_UNSUPPORTED_MESSAGE,
  });

  host.setRequestHandler("elicitation/create", async () => {
    throw Object.assign(
      new Error("method not found: No request handler configured"),
      { code: 51500 },
    );
  });
  await assert.rejects(daemon.request(request), {
    code: -32603,
    message: REMOTE_EMBEDDING_ELICITATION_UNSUPPORTED_MESSAGE,
  });

  host.setRequestHandler("elicitation/create", async () => {
    throw new Error("Downstream authorization UI failed.");
  });
  await assert.rejects(daemon.request(request), {
    code: -32603,
    message: "Downstream authorization UI failed.",
  });
});
