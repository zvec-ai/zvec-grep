import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { join } from "node:path";
import test from "node:test";
import {
  createTemporaryDirectory,
  removeTemporaryDirectory,
  runCli,
} from "../helpers/fixtures.mjs";
import { createFakeEmbeddingServer } from "../helpers/fake-embedding.mjs";

test("server-mode index reports Workspace progress", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-server-progress-",
    { cleanup: false },
  );
  const root = join(temporaryDirectory, "repo");
  const home = join(temporaryDirectory, "home");
  await mkdir(join(root, "src"), { recursive: true });
  await writeFile(
    join(root, "src", "example.ts"),
    "export const ServerProgressSymbol = 42;\n",
  );

  const endpoint = await createFakeEmbeddingServer(t);
  const port = await availablePort();
  const env = {
    HOME: home,
    NO_COLOR: "1",
    ZVEC_GREP_API_KEY: "test-key",
    ZVEC_GREP_ENDPOINT: endpoint,
    ZVEC_GREP_HOME: home,
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${port}/mcp`,
  };
  t.after(async () => {
    await runCli(["server", "off", "--home", home], {
      cwd: root,
      env,
    }).catch(() => undefined);
    await removeTemporaryDirectory(temporaryDirectory);
  });
  await runCli(
    ["server", "on", "--listen", `127.0.0.1:${port}`, "--home", home],
    { cwd: root, env },
  );

  const indexed = await runCli(
    [
      "index",
      "--mode",
      "server",
      "--embedding",
      "qwen/text-embedding-v4",
      "--allow-remote",
      "once",
      root,
    ],
    { cwd: root, env, timeout: 120_000 },
  );

  assert.match(indexed.stdout, /Workspace index: succeeded/);
  assert.match(indexed.stderr, /Scanning/);
  assert.match(indexed.stderr, /Indexing complete/);

  const dropped = await runCli(
    ["index", "--drop", "--yes", "--mode", "server", root],
    { cwd: root, env },
  );
  assert.match(dropped.stdout, /Dropped index/);

  const status = await runCli(["status", "--mode", "server", root], {
    cwd: root,
    env,
  });
  assert.match(status.stdout, /Workspace index is not configured/i);
});

test("CLI completes index, search, explicit refresh, status, and rg workflows", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-e2e-",
  );
  const root = join(temporaryDirectory, "repo");
  const home = join(temporaryDirectory, "home");
  await mkdir(join(root, "src"), { recursive: true });
  await writeFile(
    join(root, "src", "example.ts"),
    "export const FirstWorkflowSymbol = 41;\n",
  );

  const endpoint = await createFakeEmbeddingServer(t);
  const env = { HOME: home, NO_COLOR: "1" };

  const indexed = await runCli(
    [
      "index",
      "--embedding",
      "qwen/qwen3.7-text-embedding",
      "--api-key",
      "test-key",
      "--endpoint",
      endpoint,
      "--allow-remote",
      "workspace",
      "-g",
      "src/**",
      "-t",
      "ts",
      root,
    ],
    { cwd: root, env, timeout: 120_000 },
  );
  assert.match(indexed.stdout, /Workspace index/);

  const first = await runCli(
    [
      "query",
      "FirstWorkflowSymbol",
      "--limit",
      "5",
      "-g",
      "src/**",
      "-t",
      "ts",
    ],
    { cwd: root, env, timeout: 120_000 },
  );
  assert.match(first.stdout, /example\.ts/);

  await writeFile(
    join(root, "src", "example.ts"),
    "export const RefreshedWorkflowSymbol = 42;\nexport const OtherWorkflowSymbol = 43;\n",
  );
  const stale = await runCli(
    [
      "query",
      "--mode",
      "direct",
      "--fts",
      "RefreshedWorkflowSymbol",
      "--limit",
      "5",
    ],
    { cwd: root, env, timeout: 120_000 },
  );
  assert.doesNotMatch(stale.stdout, /RefreshedWorkflowSymbol/);
  assert.match(stale.stderr, /status: possibly_stale/);
  assert.match(stale.stderr, /indexing: idle \(0\/1\)/);
  await assert.rejects(
    runCli(["status", "--check-ready", root], { cwd: root, env }),
    (error) => {
      assert.match(error.stdout, /Workspace index needs an update/i);
      assert.match(error.stderr, /state: stale/i);
      return true;
    },
  );

  const background = await runCli(
    [
      "query",
      "--mode",
      "direct",
      "--refresh",
      "background",
      "--fts",
      "RefreshedWorkflowSymbol",
      "--limit",
      "5",
    ],
    { cwd: root, env, timeout: 120_000 },
  );
  assert.match(background.stderr, /requires Server mode/);
  assert.match(background.stderr, /indexing: idle \(0\/1\)/);

  const refreshed = await runCli(
    [
      "query",
      "--fts",
      "RefreshedWorkflowSymbol",
      "--limit",
      "5",
      "--refresh",
      "wait",
    ],
    {
      cwd: root,
      env,
      timeout: 120_000,
    },
  );
  assert.match(refreshed.stdout, /RefreshedWorkflowSymbol/);
  assert.doesNotMatch(refreshed.stdout, /FirstWorkflowSymbol/);

  const status = await runCli(["status", root], { cwd: root, env });
  assert.match(status.stdout, /Workspace index is ready/i);
  assert.match(status.stdout, /Coverage\s+.*100%\s+1 \/ 1 files/i);
  assert.match(status.stdout, /glob=src\/\*\*/);
  assert.match(status.stdout, /type=ts/);
  const checkedStatus = await runCli(["status", "--check-ready", root], {
    cwd: root,
    env,
  });
  assert.match(checkedStatus.stdout, /Workspace index is ready/i);

  await writeFile(
    join(root, "outside.ts"),
    "export const OutsideStoredFilterSymbol = 44;\n",
  );
  const reindexed = await runCli(["index", root], {
    cwd: root,
    env,
    timeout: 120_000,
  });
  assert.match(reindexed.stdout, /glob=src\/\*\*/);
  assert.match(reindexed.stdout, /type=ts/);
  const outside = await runCli(
    ["query", "--fts", "OutsideStoredFilterSymbol", "--refresh", "off"],
    { cwd: root, env },
  );
  assert.doesNotMatch(outside.stdout, /outside\.ts/);

  const lexical = await runCli(
    [
      "query",
      "--rg",
      "-F",
      "-i",
      "-C1",
      "-m1",
      "-g",
      "src/**",
      "-t",
      "ts",
      "RefreshedWorkflowSymbol",
      "src",
    ],
    { cwd: root, env },
  );
  assert.match(lexical.stdout, /RefreshedWorkflowSymbol/);

  const inverted = await runCli(
    ["query", "--rg", "-F", "-v", "RefreshedWorkflowSymbol", "src/example.ts"],
    { cwd: root, env },
  );
  assert.match(inverted.stdout, /OtherWorkflowSymbol/);

  const multiline = await runCli(
    [
      "query",
      "--rg",
      "-F",
      "-U",
      "RefreshedWorkflowSymbol = 42;\nexport const OtherWorkflowSymbol",
      "src/example.ts",
    ],
    { cwd: root, env },
  );
  assert.match(multiline.stdout, /example\.ts:1-2/);

  await writeFile(join(root, "patterns.txt"), "RefreshedWorkflowSymbol\n");
  const patternFile = await runCli(
    ["query", "--rg", "-f", "patterns.txt", "-t", "ts", "src"],
    { cwd: root, env },
  );
  assert.match(patternFile.stdout, /RefreshedWorkflowSymbol/);

  const dropped = await runCli(["index", root, "--drop", "--yes"], {
    cwd: root,
    env,
  });
  assert.match(dropped.stdout, /Dropped index/);
  const droppedStatus = await runCli(["status", root], { cwd: root, env });
  assert.match(droppedStatus.stdout, /Workspace index is not configured/i);
});

test("CLI exposes stable help, version, and failure behavior", async () => {
  const help = await runCli(["help"]);
  assert.match(help.stdout, /Usage:/);
  const indexHelp = await runCli(["index", "-h"]);
  assert.match(indexHelp.stdout, /qwen\/text-embedding-v4/);
  assert.doesNotMatch(indexHelp.stdout, /qwen3\.7-text-embedding/);
  const version = await runCli(["version"]);
  assert.match(version.stdout.trim(), /^\d+\.\d+\.\d+/);
  const verboseVersion = await runCli(["version", "-v"]);
  assert.equal(verboseVersion.stdout, version.stdout);
  await assert.rejects(runCli(["--definitely-invalid"]), (error) => {
    assert.equal(error.code, 1);
    assert.match(error.stderr, /Unknown command/);
    return true;
  });
  await assert.rejects(
    runCli(["collections", "list", "extra"]),
    /does not accept/,
  );
  await assert.rejects(
    runCli(["collections", "list", "--rebuild"]),
    /only be used with zg collections index/,
  );
});

async function availablePort() {
  const server = createServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  await new Promise((resolve, reject) => {
    server.close((error) => (error ? reject(error) : resolve()));
  });
  if (!address || typeof address === "string") {
    throw new Error("Could not allocate a test port.");
  }
  return address.port;
}
