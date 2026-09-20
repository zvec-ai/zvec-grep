import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { routeByMode } from "../dist/client/mode-router.js";

const execFileAsync = promisify(execFile);

test("client mode defaults to auto without explicit, environment, or global config", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-mode-default-"));
  t.after(async () => {
    await rm(home, { recursive: true, force: true });
  });
  const moduleUrl = new URL("../dist/client/mode-router.js", import.meta.url)
    .href;
  const script = `import { resolveClientMode } from ${JSON.stringify(moduleUrl)}; console.log(resolveClientMode());`;
  const env = { ...process.env, HOME: home, USERPROFILE: home };
  delete env.ZVEC_GREP_MODE;

  const result = await execFileAsync(
    process.execPath,
    ["--input-type=module", "--eval", script],
    {
      cwd: resolve("."),
      env,
    },
  );
  assert.equal(result.stdout.trim(), "auto");
});

test("auto mode chooses once before submission and never retries the selected route", async () => {
  const calls = [];
  const serverResult = await routeByMode({
    mode: "auto",
    serverAvailable: async () => true,
    server: async () => {
      calls.push("server");
      return "server";
    },
    direct: async () => {
      calls.push("direct");
      return "direct";
    },
  });
  assert.equal(serverResult, "server");
  assert.deepEqual(calls, ["server"]);

  await assert.rejects(
    routeByMode({
      mode: "auto",
      serverAvailable: async () => true,
      server: async () => {
        throw new Error("connection lost after submit");
      },
      direct: async () => "direct",
    }),
    /connection lost/,
  );

  assert.equal(
    await routeByMode({
      mode: "auto",
      serverAvailable: async () => false,
      server: async () => "server",
      direct: async () => "direct",
    }),
    "direct",
  );
});
