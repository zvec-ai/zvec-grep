import assert from "node:assert/strict";
import { request } from "node:http";
import { setImmediate } from "node:timers/promises";
import test from "node:test";
import { DaemonHttpServer } from "../dist/daemon/http-server.js";

const token = "shutdown-origin-test-token-at-least-32-characters";

async function fixture(t, host = "127.0.0.1", authentication = false) {
  let shutdowns = 0;
  const server = new DaemonHttpServer({
    host,
    port: 0,
    token: authentication ? token : undefined,
    version: "shutdown-origin-test",
    backend: {},
    onShutdown: () => {
      shutdowns++;
    },
  });
  const address = await server.start();
  t.after(() => server.close());
  const hostname = host.includes(":") ? `[${host}]` : host;
  const authority = `${hostname}:${address.port}`;
  return {
    authority,
    origin: `http://${authority}`,
    port: address.port,
    shutdowns: () => shutdowns,
    async send({
      method = "POST",
      path = "/control/shutdown",
      hostHeader = authority,
      origin,
      authorization = authentication ? `Bearer ${token}` : undefined,
      extraHeaders = [],
    } = {}) {
      const headers = ["Content-Type", "application/x-www-form-urlencoded"];
      if (hostHeader != null) headers.push("Host", hostHeader);
      if (origin !== undefined) headers.push("Origin", origin);
      if (authorization != null) {
        headers.push("Authorization", authorization);
      }
      headers.push(...extraHeaders);
      return new Promise((resolve, reject) => {
        const req = request(
          { hostname: host, port: address.port, method, path, headers },
          (response) => {
            response.resume();
            response.on("error", reject);
            response.on("end", () => resolve(response.statusCode));
          },
        );
        req.on("error", reject);
        req.setTimeout(2_000, () => req.destroy(new Error("HTTP timeout")));
        req.end();
      });
    },
  };
}

test("shutdown rejects a hostile form POST before invoking its callback", async (t) => {
  const server = await fixture(t);
  const status = await server.send({ origin: "https://untrusted.example" });
  await setImmediate();
  assert.deepEqual(
    { status, shutdowns: server.shutdowns() },
    {
      status: 403,
      shutdowns: 0,
    },
  );
});

for (const host of ["127.0.0.1", "::1"]) {
  for (const authentication of [false, true]) {
    test(`shutdown enforces exact origins on ${host}, token ${authentication ? "on" : "off"}`, async (t) => {
      const server = await fixture(t, host, authentication);
      const otherPort = server.port === 65535 ? 65534 : server.port + 1;
      const localOrigin = `http://localhost:${server.port}`;
      const cases = [
        ["native client", {}, 202],
        ["same origin", { origin: server.origin }, 202],
        [
          "localhost native alias",
          { hostHeader: `localhost:${server.port}` },
          202,
        ],
        [
          "localhost same origin",
          { hostHeader: `localhost:${server.port}`, origin: localOrigin },
          202,
        ],
        [
          "normalized origin",
          {
            hostHeader: `LOCALHOST:${server.port}`,
            origin: `HTTP://LOCALHOST:${server.port}`,
          },
          202,
        ],
        ["external origin", { origin: "https://untrusted.example" }, 403],
        ["other local hostname", { origin: localOrigin }, 403],
        ["other local port", { origin: `http://localhost:${otherPort}` }, 403],
        [
          "other port with matching Host",
          {
            hostHeader: `localhost:${otherPort}`,
            origin: `http://localhost:${otherPort}`,
          },
          403,
        ],
        [
          "missing port on nondefault listener",
          { hostHeader: "localhost" },
          403,
        ],
        [
          "other scheme",
          { origin: server.origin.replace("http:", "https:") },
          403,
        ],
        ["opaque origin", { origin: "null" }, 403],
        ["empty origin", { origin: "" }, 403],
        ["invalid origin", { origin: "not-an-origin" }, 403],
        ["invalid origin port", { origin: "http://localhost:65536" }, 403],
        ["origin path", { origin: `${server.origin}/` }, 403],
        ["origin query", { origin: `${server.origin}?` }, 403],
        ["origin fragment", { origin: `${server.origin}#` }, 403],
        [
          "origin credentials",
          { origin: server.origin.replace("http://", "http://user@") },
          403,
        ],
        [
          "duplicate Origin",
          { origin: server.origin, extraHeaders: ["Origin", server.origin] },
          403,
        ],
        ["duplicate Host", { extraHeaders: ["Host", server.authority] }, 403],
        ["external Host", { hostHeader: "untrusted.example" }, 403],
        ["missing Host", { hostHeader: null }, [400, 403]],
        ["empty Host", { hostHeader: "" }, [400, 403]],
        ["invalid Host", { hostHeader: "[" }, 403],
        ["invalid Host port", { hostHeader: "localhost:65536" }, 403],
        ["Host credentials", { hostHeader: `user@${server.authority}` }, 403],
        ["Host path", { hostHeader: `${server.authority}/` }, 403],
        ["Host query", { hostHeader: `${server.authority}?` }, 403],
        ["Host fragment", { hostHeader: `${server.authority}#` }, 403],
        ["Host backslash", { hostHeader: `${server.authority}\\ignored` }, 403],
        ["GET", { method: "GET" }, 405],
        ["HEAD", { method: "HEAD" }, 405],
        ["OPTIONS", { method: "OPTIONS", origin: server.origin }, 405],
        ["DELETE", { method: "DELETE" }, 405],
      ];
      if (authentication) {
        cases.push(
          ["missing token", { authorization: null }, 401],
          ["empty token", { authorization: "" }, 401],
          ["incorrect token", { authorization: "Bearer incorrect" }, 401],
          [
            "same origin without token",
            { origin: server.origin, authorization: null },
            401,
          ],
        );
      }
      for (const [name, options, expected] of cases) {
        await t.test(name, async () => {
          const before = server.shutdowns();
          const status = await server.send(options);
          await setImmediate();
          assert.ok(
            [expected].flat().includes(status),
            `expected ${expected}, got ${status}`,
          );
          assert.equal(server.shutdowns(), before + (expected === 202 ? 1 : 0));
        });
      }
    });
  }
}

test("MCP retains loopback-origin compatibility with shared strict header parsing", async (t) => {
  const server = await fixture(t, "127.0.0.1", true);
  for (const path of ["/mcp", "/mcp/admin"]) {
    for (const origin of [
      undefined,
      server.origin,
      "http://localhost:3000",
      "http://[::1]:3001",
    ]) {
      assert.equal(await server.send({ method: "PATCH", path, origin }), 405);
    }
    for (const origin of [
      "null",
      "",
      "https://untrusted.example",
      `${server.origin}/`,
      `${server.origin}?`,
      `${server.origin}#`,
      `http://user@${server.authority}`,
    ]) {
      assert.equal(await server.send({ method: "PATCH", path, origin }), 403);
    }
    assert.equal(
      await server.send({
        method: "PATCH",
        path,
        extraHeaders: ["Host", server.authority],
      }),
      403,
    );
    assert.equal(
      await server.send({ method: "PATCH", path, hostHeader: "[" }),
      403,
    );
    assert.equal(
      await server.send({ method: "PATCH", path, authorization: "" }),
      401,
    );
  }
  assert.equal(await server.send({ method: "GET", path: "/healthz" }), 200);
  assert.equal(server.shutdowns(), 0);
});
