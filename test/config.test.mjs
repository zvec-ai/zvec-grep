import assert from "node:assert/strict";
import { mkdtemp, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  readGlobalConfig,
  resolveEmbeddingRuntimeOptions,
  updateGlobalConfig,
} from "../dist/engine/config.js";
import { resolveEmbeddingReference } from "../dist/engine/models/index.js";

test("global config v1 is created securely and merged incrementally", async (t) => {
  const temporaryDirectory = await mkdtemp(join(tmpdir(), "zvec-grep-config-"));
  const configPath = join(temporaryDirectory, ".zvec-grep", "config.json");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  assert.deepEqual(readGlobalConfig(configPath), { version: 1 });

  updateGlobalConfig(
    {
      defaults: {
        embedding: "qwen/text-embedding-v4",
        modelCacheDir: "/tmp/models",
      },
      providers: {
        qwen: {
          apiKey: "first-key",
        },
        dgx: {
          auth: "none",
        },
      },
      models: {
        "qwen/text-embedding-v4": {
          endpoint: "https://example.test/embeddings",
        },
        "local/embeddinggemma-300m": {
          device: "metal",
        },
      },
      client: { mode: "server", serverUrl: "http://127.0.0.1:8123/mcp" },
      server: { host: "127.0.0.1", port: 8123 },
    },
    configPath,
  );
  updateGlobalConfig(
    {
      providers: {
        qwen: {
          apiKey: "rotated-key",
        },
      },
      client: { mode: "auto" },
    },
    configPath,
  );

  assert.deepEqual(readGlobalConfig(configPath), {
    version: 1,
    defaults: {
      embedding: "qwen/text-embedding-v4",
      modelCacheDir: "/tmp/models",
    },
    providers: {
      qwen: {
        apiKey: "rotated-key",
      },
      dgx: {
        auth: "none",
      },
    },
    models: {
      "qwen/text-embedding-v4": {
        endpoint: "https://example.test/embeddings",
      },
      "local/embeddinggemma-300m": {
        device: "metal",
      },
    },
    client: { mode: "auto", serverUrl: "http://127.0.0.1:8123/mcp" },
    server: { host: "127.0.0.1", port: 8123 },
  });

  if (process.platform !== "win32") {
    const [directoryInfo, fileInfo] = await Promise.all([
      stat(join(temporaryDirectory, ".zvec-grep")),
      stat(configPath),
    ]);
    assert.equal(directoryInfo.mode & 0o777, 0o700);
    assert.equal(fileInfo.mode & 0o777, 0o600);
  }
});

test("embedding runtime resolver preserves every precedence layer", () => {
  const config = {
    version: 1,
    providers: { qwen: { apiKey: "global-key" } },
    models: {
      "qwen/text-embedding-v4": {
        endpoint: "https://global.test/embeddings",
      },
      "local/embeddinggemma-300m": { device: "metal" },
    },
  };
  const environment = {
    ZVEC_GREP_API_KEY: "env-key",
    ZVEC_GREP_ENDPOINT: "https://env.test/embeddings",
    ZVEC_GREP_DEVICE: "cpu",
  };

  assert.deepEqual(
    resolveEmbeddingRuntimeOptions(
      "qwen/text-embedding-v4",
      {
        apiKey: "request-key",
        endpoint: "https://request.test/embeddings",
      },
      {
        apiKey: "workspace-key",
        endpoint: "https://workspace.test/embeddings",
      },
      config,
      environment,
    ),
    {
      apiKey: "request-key",
      endpoint: "https://request.test/embeddings",
    },
  );
  assert.deepEqual(
    resolveEmbeddingRuntimeOptions(
      "qwen/text-embedding-v4",
      {},
      {
        apiKey: "workspace-key",
        endpoint: "https://workspace.test/embeddings",
      },
      config,
      environment,
    ),
    {
      apiKey: "workspace-key",
      endpoint: "https://workspace.test/embeddings",
    },
  );
  assert.deepEqual(
    resolveEmbeddingRuntimeOptions(
      "qwen/text-embedding-v4",
      {},
      {},
      config,
      environment,
    ),
    {
      apiKey: "global-key",
      endpoint: "https://global.test/embeddings",
    },
  );
  assert.deepEqual(
    resolveEmbeddingRuntimeOptions(
      "local/embeddinggemma-300m",
      {},
      {},
      config,
      environment,
    ),
    {
      apiKey: "env-key",
      device: "metal",
    },
  );
});

test("explicit unauthenticated provider config suppresses only environment credentials", () => {
  const config = {
    version: 1,
    providers: { dgx: { auth: "none" } },
  };
  const environment = { ZVEC_GREP_API_KEY: "unrelated-environment-key" };

  assert.deepEqual(
    resolveEmbeddingRuntimeOptions(
      "dgx/qwen3-embedding-0.6b",
      {},
      {},
      config,
      environment,
    ),
    { apiKey: "" },
  );
  assert.deepEqual(
    resolveEmbeddingRuntimeOptions(
      "dgx/qwen3-embedding-0.6b",
      { apiKey: "explicit-key" },
      {},
      config,
      environment,
    ),
    { apiKey: "explicit-key" },
  );
  assert.deepEqual(
    resolveEmbeddingRuntimeOptions(
      "dgx/qwen3-embedding-0.6b",
      {},
      { apiKey: "workspace-key" },
      config,
      environment,
    ),
    { apiKey: "workspace-key" },
  );
});

test("provider authentication updates replace mutually exclusive credentials", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-config-auth-"),
  );
  const configPath = join(temporaryDirectory, ".zvec-grep", "config.json");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  updateGlobalConfig({ providers: { dgx: { apiKey: "secret" } } }, configPath);
  updateGlobalConfig({ providers: { dgx: { auth: "none" } } }, configPath);
  assert.deepEqual(readGlobalConfig(configPath).providers?.dgx, {
    auth: "none",
  });

  updateGlobalConfig(
    { providers: { dgx: { apiKey: "replacement" } } },
    configPath,
  );
  assert.deepEqual(readGlobalConfig(configPath).providers?.dgx, {
    apiKey: "replacement",
  });
});

test("embedding reference resolver prefers the environment over the global default", () => {
  assert.equal(
    resolveEmbeddingReference({
      explicit: "qwen/qwen3.7-text-embedding",
      existing: "local/qwen3-embedding-0.6b",
      environment: {
        ZVEC_GREP_EMBEDDING: "local/embeddinggemma-300m",
      },
      globalDefault: "local/potion-code-16m-v2",
    }),
    "qwen/qwen3.7-text-embedding",
  );
  assert.equal(
    resolveEmbeddingReference({
      environment: {
        ZVEC_GREP_EMBEDDING: "local/embeddinggemma-300m",
      },
      globalDefault: "local/potion-code-16m-v2",
    }),
    "local/embeddinggemma-300m",
  );
  assert.equal(
    resolveEmbeddingReference({
      environment: { ZVEC_GREP_EMBEDDING: "   " },
      globalDefault: "local/potion-code-16m-v2",
    }),
    "local/potion-code-16m-v2",
  );
  assert.equal(
    resolveEmbeddingReference({
      fallback: "local/potion-code-16m-v2",
    }),
    "local/potion-code-16m-v2",
  );
});

test("embedding reference resolver validates only a selected environment model", () => {
  assert.throws(
    () =>
      resolveEmbeddingReference({
        environment: { ZVEC_GREP_EMBEDDING: "unknown/model" },
        globalDefault: "local/potion-code-16m-v2",
      }),
    /Invalid ZVEC_GREP_EMBEDDING: unsupported model unknown\/model/,
  );
  assert.throws(
    () =>
      resolveEmbeddingReference({
        environment: { ZVEC_GREP_EMBEDDING: "unknown/model" },
      }),
    /zg --help models/,
  );
  assert.equal(
    resolveEmbeddingReference({
      existing: "local/embeddinggemma-300m",
      environment: { ZVEC_GREP_EMBEDDING: "unknown/model" },
      globalDefault: "local/potion-code-16m-v2",
    }),
    "local/embeddinggemma-300m",
  );
});

test("global config v1 rejects malformed schemas without echoing secrets", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-config-invalid-"),
  );
  const configPath = join(temporaryDirectory, "config.json");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await writeFile(
    configPath,
    JSON.stringify({ version: 1, defaults: { device: "cpu" } }),
  );
  assert.throws(
    () => readGlobalConfig(configPath),
    (error) => {
      assert.match(error.context, /defaults\.device is not supported/);
      return true;
    },
  );

  await writeFile(
    configPath,
    JSON.stringify({
      version: 1,
      providers: { dgx: { apiKey: "secret", auth: "none" } },
    }),
  );
  assert.throws(
    () => readGlobalConfig(configPath),
    (error) => {
      assert.match(error.context, /cannot set both apiKey and auth none/);
      return true;
    },
  );

  await writeFile(
    configPath,
    JSON.stringify({ version: 1, providers: { qwen: { auth: "none" } } }),
  );
  assert.throws(
    () => readGlobalConfig(configPath),
    (error) => {
      assert.match(error.context, /provider requires an API key/);
      return true;
    },
  );

  await writeFile(
    configPath,
    JSON.stringify({
      version: 1,
      providers: { qwen: { apiKey: 12345 } },
    }),
  );
  assert.throws(
    () => readGlobalConfig(configPath),
    (error) => {
      assert.equal(error.code, "ZVEC_GREP.ENGINE.CONFIG.INVALID");
      assert.doesNotMatch(error.message, /12345/);
      assert.doesNotMatch(error.context, /12345/);
      return true;
    },
  );

  for (const models of [
    { "qwen/text-embedding-v4": { device: "metal" } },
    {
      "local/embeddinggemma-300m": {
        endpoint: "https://example.test/embeddings",
      },
    },
    { "qwen/text-embedding-v4": { endpoint: "not-a-url" } },
  ]) {
    await writeFile(configPath, JSON.stringify({ version: 1, models }));
    assert.throws(() => readGlobalConfig(configPath));
  }
});
