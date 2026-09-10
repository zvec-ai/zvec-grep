import assert from "node:assert/strict";
import { readFile, stat } from "node:fs/promises";
import test from "node:test";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import {
  ensureSearchServer,
  ensureImplicitTokenFile,
  implicitListenAddress,
  IMPLICIT_SERVER_START_TIMEOUT_MS,
} from "../../dist/client/ensure-server.js";

test("implicit startup only accepts plain local MCP endpoints", () => {
  assert.equal(
    implicitListenAddress("http://127.0.0.1:8123/mcp"),
    "127.0.0.1:8123",
  );
  assert.equal(implicitListenAddress("http://[::1]:8123/mcp"), "[::1]:8123");
  for (const url of [
    "bad",
    "https://localhost/mcp",
    "http://example.com/mcp",
    "http://127.0.0.1/other",
    "http://user:secret@localhost/mcp",
    "http://localhost/mcp?secret=x",
  ]) {
    assert.equal(implicitListenAddress(url), undefined);
  }
});

test("implicit daemon reuses an owned ready endpoint and does not hijack others", async () => {
  let starts = 0;
  let tokens = 0;
  const serverUrl = "http://127.0.0.1:8123/mcp";
  const deps = {
    serverStatus: async () => ({ running: true, ready: true, serverUrl }),
    startServer: async () => {
      starts++;
    },
    ensureImplicitTokenFile: async () => {
      tokens++;
    },
  };
  assert.equal(
    await ensureSearchServer({ cliPath: "zg", serverUrl }, deps),
    serverUrl,
  );
  assert.equal(
    await ensureSearchServer(
      { cliPath: "zg", serverUrl: "http://127.0.0.1:8999/mcp" },
      deps,
    ),
    undefined,
  );
  assert.equal(starts, 0);
  assert.equal(tokens, 0);
});

test("startup has a bounded wait and errors permit a direct fallback", async () => {
  const serverUrl = "http://127.0.0.1:8123/mcp";
  let warning = 0;
  const options = {
    cliPath: "zg",
    serverUrl,
    home: "/tmp/home",
    tokenFile: "/tmp/token",
    onUnavailable: () => warning++,
  };
  const deps = {
    serverStatus: async () => ({ running: false, ready: false }),
    ensureImplicitTokenFile: async () => {
      throw new Error("must respect explicit token");
    },
    startServer: async (input) => {
      assert.equal(input.timeoutMs, IMPLICIT_SERVER_START_TIMEOUT_MS);
      assert.equal(input.listen, "127.0.0.1:8123");
      assert.equal(input.tokenFile, "/tmp/token");
      return { ready: true, serverUrl };
    },
  };
  assert.equal(await ensureSearchServer(options, deps), serverUrl);
  deps.startServer = async () => {
    throw new Error("private diagnostic");
  };
  assert.equal(await ensureSearchServer(options, deps), undefined);
  assert.equal(warning, 1);
});

test("concurrent implicit starters publish one private token without rotation", async (t) => {
  const home = await createTemporaryDirectory(t, "zvec-implicit-token-");
  const paths = await Promise.all(
    Array.from({ length: 12 }, () => ensureImplicitTokenFile(home)),
  );
  assert.equal(new Set(paths).size, 1);
  const token = await readFile(paths[0], "utf8");
  assert.match(token, /^[a-f0-9]{64}\n$/);
  await ensureImplicitTokenFile(home);
  assert.equal(await readFile(paths[0], "utf8"), token);
  if (process.platform !== "win32")
    assert.equal((await stat(paths[0])).mode & 0o777, 0o600);
});
