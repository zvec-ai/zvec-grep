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
