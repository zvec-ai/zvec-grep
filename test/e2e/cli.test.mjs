import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import test from "node:test";
import {
  createTemporaryDirectory,
  removeTemporaryDirectory,
  runCli,
} from "../helpers/fixtures.mjs";
import { createFakeEmbeddingServer } from "../helpers/fake-embedding.mjs";

test("direct and server indexes report aggregate local model download progress", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-local-download-progress-",
    { cleanup: false },
  );
  const root = join(temporaryDirectory, "repo");
  const home = join(temporaryDirectory, "home");
  const directCache = join(temporaryDirectory, "direct-models");
  const serverCache = join(temporaryDirectory, "server-models");
  await mkdir(join(root, "src"), { recursive: true });
  await writeFile(
    join(root, "src", "example.ts"),
    "export const LocalDownloadProgress = 1;\n",
  );

  const preload = pathToFileURL(
    resolve("test/helpers/fake-model-download.mjs"),
  ).href;
  const port = await availablePort();
  const baseEnv = {
    HOME: home,
    USERPROFILE: home,
    NO_COLOR: "1",
    NODE_OPTIONS: `--import=${preload}`,
    ZVEC_GREP_HOME: home,
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${port}/mcp`,
  };

  await assert.rejects(
    runCli(
      [
        "index",
        "--mode",
        "direct",
        "--embedding",
        "local/potion-base-8m",
        "--model-cache",
        directCache,
        root,
      ],
      { cwd: root, env: baseEnv, timeout: 120_000 },
    ),
    (error) => {
      assert.match(error.stderr, /Preparing local\/potion-base-8m/);
      assert.match(error.stderr, /Downloading local\/potion-base-8m/);
      assert.doesNotMatch(error.stderr, /model\.safetensors|tokenizer\.json/);
      return true;
    },
  );

  const serverEnv = {
    ...baseEnv,
    ZVEC_GREP_MODEL_CACHE: serverCache,
  };
  t.after(async () => {
    await runCli(["server", "off", "--home", home], {
      cwd: root,
      env: serverEnv,
    }).catch(() => undefined);
    await removeTemporaryDirectory(temporaryDirectory);
  });
  await runCli(
    ["server", "on", "--listen", `127.0.0.1:${port}`, "--home", home],
    { cwd: root, env: serverEnv },
  );
  await assert.rejects(
    runCli(
      [
        "index",
        "--mode",
        "server",
        "--embedding",
        "local/potion-base-8m",
        root,
      ],
      { cwd: root, env: serverEnv, timeout: 120_000 },
    ),
    (error) => {
      assert.match(error.stderr, /Preparing local\/potion-base-8m/);
      assert.match(error.stderr, /Downloading local\/potion-base-8m/);
      assert.doesNotMatch(error.stderr, /model\.safetensors|tokenizer\.json/);
      return true;
    },
  );
});

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
    USERPROFILE: home,
    NO_COLOR: "1",
    ZVEC_GREP_API_KEY: "test-key",
    ZVEC_GREP_ENDPOINT: endpoint,
    ZVEC_GREP_HOME: home,
    ZVEC_GREP_SERVER_URL: `http://127.0.0.1:${port}/mcp`,
  };
  await mkdir(join(home, ".zvec-grep"), { recursive: true });
  await writeFile(
    join(home, ".zvec-grep", "config.json"),
    `${JSON.stringify({
      version: 1,
      defaults: { embedding: "qwen/text-embedding-v4" },
    })}\n`,
  );
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
    ["index", "--mode", "server", "--allow-remote", root],
    {
      cwd: root,
      env: {
        ...env,
        ZVEC_GREP_EMBEDDING: "qwen/qwen3.7-text-embedding",
      },
      timeout: 120_000,
    },
  );

  assert.match(indexed.stdout, /Workspace index: succeeded/);
  assert.match(indexed.stderr, /Scanning/);
  assert.match(indexed.stderr, /Indexing complete/);
  const indexedStatus = await runCli(["status", "--mode", "server", root], {
    cwd: root,
    env,
  });
  assert.match(indexedStatus.stdout, /qwen\/qwen3\.7-text-embedding/);
  await runCli(["index", "--mode", "server", "--allow-remote", root], {
    cwd: root,
    env: {
      ...env,
      ZVEC_GREP_EMBEDDING: "qwen/text-embedding-v4",
    },
    timeout: 120_000,
  });
  const reusedStatus = await runCli(["status", "--mode", "server", root], {
    cwd: root,
    env,
  });
  assert.match(reusedStatus.stdout, /qwen\/qwen3\.7-text-embedding/);
  await assert.rejects(
    runCli(
      [
        "index",
        "--mode",
        "server",
        "--embedding",
        "qwen/text-embedding-v4",
        "--allow-remote",
        root,
      ],
      { cwd: root, env, timeout: 120_000 },
    ),
    /does not match requested model.*use rebuild/i,
  );
  await runCli(
    [
      "index",
      "--mode",
      "server",
      "--embedding",
      "qwen/text-embedding-v4",
      "--rebuild",
      "--allow-remote",
      root,
    ],
    { cwd: root, env, timeout: 120_000 },
  );
  const rebuiltStatus = await runCli(["status", "--mode", "server", root], {
    cwd: root,
    env,
  });
  assert.match(rebuiltStatus.stdout, /qwen\/text-embedding-v4/);

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
  const env = {
    HOME: home,
    USERPROFILE: home,
    NO_COLOR: "1",
    ZVEC_GREP_EMBEDDING: "qwen/qwen3.7-text-embedding",
  };
  await mkdir(join(home, ".zvec-grep"), { recursive: true });
  await writeFile(
    join(home, ".zvec-grep", "config.json"),
    `${JSON.stringify({
      version: 1,
      defaults: { embedding: "qwen/text-embedding-v4" },
    })}\n`,
  );

  const indexed = await runCli(
    [
      "index",
      "--api-key",
      "test-key",
      "--endpoint",
      endpoint,
      "--allow-remote",
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
      "--allow-remote",
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
      "--allow-remote",
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
  assert.match(status.stdout, /qwen\/qwen3\.7-text-embedding/);
  assert.match(status.stdout, /Coverage\s+.*100%\s+1 \/ 1 files/i);
  assert.match(status.stdout, /glob=src\/\*\*/);
  assert.match(status.stdout, /type=ts/);
  const existingAuth = await runCli(
    [
      "auth",
      "grant",
      root,
      "--capability",
      "embedding",
      "--scope",
      "workspace",
    ],
    {
      cwd: root,
      env: {
        ...env,
        ZVEC_GREP_API_KEY: "test-key",
        ZVEC_GREP_EMBEDDING: "qwen/text-embedding-v4",
      },
    },
  );
  assert.match(existingAuth.stdout, /qwen\/qwen3\.7-text-embedding/);
  assert.doesNotMatch(existingAuth.stdout, /qwen\/text-embedding-v4/);
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
  assert.match(multiline.stdout, /example\.ts\n/);
  assert.match(multiline.stdout, / {2}1:.*RefreshedWorkflowSymbol/);
  assert.match(multiline.stdout, / {2}2:.*OtherWorkflowSymbol/);

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

test("auth grant uses the environment model before the global default", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-auth-environment-",
  );
  const root = join(temporaryDirectory, "repo");
  const home = join(temporaryDirectory, "home");
  await mkdir(join(root, "src"), { recursive: true });
  await mkdir(join(home, ".zvec-grep"), { recursive: true });
  await writeFile(
    join(home, ".zvec-grep", "config.json"),
    `${JSON.stringify({
      version: 1,
      defaults: { embedding: "qwen/text-embedding-v4" },
    })}\n`,
  );
  const env = {
    HOME: home,
    USERPROFILE: home,
    NO_COLOR: "1",
    ZVEC_GREP_API_KEY: "test-key",
    ZVEC_GREP_EMBEDDING: "qwen/qwen3.7-text-embedding",
  };

  const granted = await runCli(
    [
      "auth",
      "grant",
      root,
      "--capability",
      "embedding",
      "--scope",
      "workspace",
    ],
    { cwd: root, env },
  );

  assert.match(granted.stdout, /qwen\/qwen3\.7-text-embedding/);
  assert.doesNotMatch(granted.stdout, /qwen\/text-embedding-v4/);
});

test("direct index points an empty workspace to file type help", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-empty-index-",
  );
  const root = join(temporaryDirectory, "repo");
  const home = join(temporaryDirectory, "home");
  await mkdir(root, { recursive: true });

  const indexed = await runCli(
    ["index", "--mode", "direct", "--embedding", "local/potion-base-8m", root],
    {
      cwd: root,
      env: {
        HOME: home,
        USERPROFILE: home,
        NO_COLOR: "1",
        ZVEC_GREP_HOME: home,
      },
    },
  );

  assert.match(indexed.stdout, /0 scanned/);
  assert.match(indexed.stdout, /zg help file-types/);
});

test("CLI exposes stable help, version, and failure behavior", async (t) => {
  const help = await runCli(["help"]);
  assert.match(help.stdout, /Usage:/);
  assert.match(help.stdout, /zg help models or zg help file-types/);
  assert.match(help.stdout, /zg help environment/);
  assert.match(help.stdout, /ZVEC_GREP_MODE/);
  const helpTopics = await runCli(["help", "help"]);
  assert.match(helpTopics.stdout, /models\s+Supported embedding models/);
  assert.match(helpTopics.stdout, /file-types\s+Supported file types/);
  const modelsHelp = await runCli(["help", "models"]);
  assert.match(modelsHelp.stdout, /local\/potion-code-16m-v2/);
  assert.match(modelsHelp.stdout, /qwen\/qwen3-vl-embedding/);
  assert.match(modelsHelp.stdout, /Local models are downloaded/);
  assert.match(modelsHelp.stdout, /Workspace authorization/);
  const fileTypesHelp = await runCli(["help", "file-types"]);
  assert.match(fileTypesHelp.stdout, /Structured code \(symbols and scopes\)/);
  assert.match(fileTypesHelp.stdout, /typescript\s+\.ts/);
  assert.match(fileTypesHelp.stdout, /Other code \(plain-text chunks\)/);
  assert.match(fileTypesHelp.stdout, /dockerfile\s+Dockerfile/);
  assert.match(fileTypesHelp.stdout, /Documents and data/);
  assert.match(
    fileTypesHelp.stdout,
    /Images \(multimodal embedding required\)/,
  );
  assert.match(fileTypesHelp.stdout, /\.pdf/);
  assert.match(fileTypesHelp.stdout, /Code\s+1 MiB/);
  assert.match(fileTypesHelp.stdout, /Text\s+256 MiB/);
  const indexHelp = await runCli(["index", "-h"]);
  assert.match(indexHelp.stdout, /qwen\/text-embedding-v4/);
  assert.doesNotMatch(indexHelp.stdout, /qwen3\.7-text-embedding/);
  assert.match(indexHelp.stdout, /ZVEC_GREP_EMBEDDING/);
  const configHelp = await runCli(["config", "--help"]);
  assert.match(configHelp.stdout, /Default API key for the provider/);
  assert.match(configHelp.stdout, /Existing indexes continue to use/);
  const authHelp = await runCli(["auth", "--help"]);
  assert.match(
    authHelp.stdout,
    /selects the Remote Embedding model to authorize/,
  );
  assert.match(authHelp.stdout, /existing Workspace index model/);
  assert.match(authHelp.stdout, /ZVEC_GREP_EMBEDDING, then the global default/);
  const environmentHelp = await runCli(["help", "environment"], {
    env: {
      ...process.env,
      ZVEC_GREP_API_KEY: "environment-help-secret",
      ZVEC_GREP_SERVER_TOKEN: "server-help-secret".repeat(3),
    },
  });
  assert.match(environmentHelp.stdout, /ZVEC_GREP_AUTHORIZATION_KEY_FILE/);
  assert.match(environmentHelp.stdout, /DASHSCOPE_API_KEY/);
  assert.match(environmentHelp.stdout, /CLI > Workspace snapshot/);
  assert.match(
    environmentHelp.stdout,
    /--embedding > ZVEC_GREP_EMBEDDING > Global config/,
  );
  assert.match(environmentHelp.stdout, /forwards its ZVEC_GREP_EMBEDDING/);
  assert.doesNotMatch(environmentHelp.stdout, /environment-help-secret/);
  assert.doesNotMatch(environmentHelp.stdout, /server-help-secret/);
  const environmentAliasHelp = await runCli(["help", "env"]);
  assert.equal(environmentAliasHelp.stdout, environmentHelp.stdout);
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
    runCli(["index", "--embedding", "unknown/model"]),
    (error) => {
      assert.equal(error.code, 1);
      assert.match(error.stderr, /Unsupported embedding model: unknown\/model/);
      assert.match(error.stderr, /zg help models/);
      return true;
    },
  );
  const invalidEnvironmentRoot = await createTemporaryDirectory(
    t,
    "zvec-grep-invalid-embedding-environment-",
  );
  await assert.rejects(
    runCli(["index", invalidEnvironmentRoot, "--mode", "direct"], {
      cwd: invalidEnvironmentRoot,
      env: { ZVEC_GREP_EMBEDDING: "unknown/model" },
    }),
    (error) => {
      assert.match(
        error.stderr,
        /Invalid ZVEC_GREP_EMBEDDING: unsupported model unknown\/model/,
      );
      assert.match(error.stderr, /zg help models/);
      return true;
    },
  );
  await assert.rejects(runCli(["collections"]), /Unknown command/);
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
