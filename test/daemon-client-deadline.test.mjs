import assert from "node:assert/strict";
import { createServer } from "node:http";
import test from "node:test";
import {
  DaemonClient,
  DaemonCallTimeoutError,
} from "../dist/client/daemon-client.js";
import { DaemonHttpServer } from "../dist/daemon/http-server.js";
import { createTemporaryDirectory } from "./helpers/fixtures.mjs";

test("daemon request deadline cancels a stalled handshake and restores process handlers", async (t) => {
  const home = await createTemporaryDirectory(t, "zvec-client-deadline-");
  const server = createServer(() => {});
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(async () => {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  });
  const client = new DaemonClient({
    serverUrl: `http://127.0.0.1:${server.address().port}/mcp`,
    home,
  });
  const listeners = process.listenerCount("SIGINT");
  const start = performance.now();
  await assert.rejects(
    client.callTool(
      "zvec_grep_index_status",
      { root: home },
      { timeoutMs: 100 },
    ),
    DaemonCallTimeoutError,
  );
  assert.ok(
    performance.now() - start < 2_000,
    "handshake must not consume the SDK's 60-second timeout",
  );
  assert.equal(process.listenerCount("SIGINT"), listeners);
});

test("daemon request deadline also covers a stalled tool, without labeling it user cancellation", async (t) => {
  const home = await createTemporaryDirectory(t, "zvec-client-tool-deadline-");
  let release;
  const barrier = new Promise((resolve) => {
    release = resolve;
  });
  let called = false;
  const server = new DaemonHttpServer({
    host: "127.0.0.1",
    port: 0,
    version: "test",
    backend: {
      indexStatus: async () => {
        called = true;
        await barrier;
        return {};
      },
    },
  });
  const address = await server.start();
  t.after(async () => {
    release();
    await server.close();
  });
  const client = new DaemonClient({
    serverUrl: `http://127.0.0.1:${address.port}/mcp`,
    home,
  });
  const started = performance.now();
  await assert.rejects(
    client.callTool(
      "zvec_grep_index_status",
      { root: home },
      { timeoutMs: 300 },
    ),
    DaemonCallTimeoutError,
  );
  assert.equal(called, true);
  assert.ok(
    performance.now() - started < 2_000,
    "tool wait must honor its deadline before fixture cleanup",
  );
});
