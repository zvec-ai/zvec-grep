import assert from "node:assert/strict";
import { mkdtemp, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  readGlobalConfig,
  resolveEmbeddingRuntimeOptions,
  updateGlobalConfig,
  updateGlobalConfigFromExplicitOptions,
} from "../dist/engine/config.js";

test("global config is created securely and merged incrementally", async (t) => {
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
        device: "cpu",
      },
      providers: {
        qwen: {
          apiKey: "first-key",
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
          endpoint: "https://example.test/embeddings",
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
      device: "cpu",
    },
    providers: {
      qwen: {
        apiKey: "first-key",
        endpoint: "https://example.test/embeddings",
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

test("explicit index options become provider-aware global config", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-config-explicit-"),
  );
  const configPath = join(temporaryDirectory, ".zvec-grep", "config.json");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  assert.equal(
    updateGlobalConfigFromExplicitOptions(
      {
        embedding: "qwen/text-embedding-v4",
        apiKey: "explicit-key",
        endpoint: "https://example.test/embeddings",
      },
      undefined,
      configPath,
    ),
    true,
  );
  assert.deepEqual(readGlobalConfig(configPath), {
    version: 1,
    defaults: {
      embedding: "qwen/text-embedding-v4",
    },
    providers: {
      qwen: {
        apiKey: "explicit-key",
        endpoint: "https://example.test/embeddings",
      },
    },
  });
  assert.equal(
    updateGlobalConfigFromExplicitOptions({}, "qwen", configPath),
    false,
  );
});

test("local embedding options are persisted per model", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-config-model-"),
  );
  const configPath = join(temporaryDirectory, ".zvec-grep", "config.json");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  assert.equal(
    updateGlobalConfigFromExplicitOptions(
      {
        embedding: "local/embeddinggemma-300m",
        device: "metal",
      },
      undefined,
      configPath,
    ),
    true,
  );
  assert.equal(
    updateGlobalConfigFromExplicitOptions(
      {
        embedding: "local/qwen3-embedding-0.6b",
        device: "cpu",
      },
      undefined,
      configPath,
    ),
    true,
  );

  assert.deepEqual(readGlobalConfig(configPath), {
    version: 1,
    defaults: {
      embedding: "local/qwen3-embedding-0.6b",
    },
    models: {
      "local/embeddinggemma-300m": {
        device: "metal",
      },
      "local/qwen3-embedding-0.6b": {
        device: "cpu",
      },
    },
  });
});

test("existing local index persists runtime options for its stored model", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-config-existing-model-"),
  );
  const configPath = join(temporaryDirectory, ".zvec-grep", "config.json");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  assert.equal(
    updateGlobalConfigFromExplicitOptions(
      { device: "metal" },
      "local/embeddinggemma-300m",
      configPath,
    ),
    true,
  );
  assert.deepEqual(readGlobalConfig(configPath), {
    version: 1,
    models: {
      "local/embeddinggemma-300m": { device: "metal" },
    },
  });
});

test("model runtime settings override defaults and do not affect remote models", () => {
  const config = {
    version: 1,
    defaults: { device: "cpu" },
    models: {
      "local/embeddinggemma-300m": {
        device: "metal",
      },
    },
  };

  assert.deepEqual(
    resolveEmbeddingRuntimeOptions("local/embeddinggemma-300m", {}, config),
    { device: "metal" },
  );
  assert.deepEqual(
    resolveEmbeddingRuntimeOptions(
      "local/embeddinggemma-300m",
      { device: "cpu" },
      config,
    ),
    { device: "cpu" },
  );
  assert.deepEqual(
    resolveEmbeddingRuntimeOptions("qwen/text-embedding-v4", {}, config),
    {},
  );
});

test("global config rejects malformed fields without echoing secrets", async (t) => {
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-config-invalid-"),
  );
  const configPath = join(temporaryDirectory, "config.json");
  t.after(async () => {
    await rm(temporaryDirectory, { recursive: true, force: true });
  });

  await writeFile(
    configPath,
    JSON.stringify({
      version: 1,
      providers: {
        qwen: {
          apiKey: 12345,
        },
      },
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

  await writeFile(
    configPath,
    JSON.stringify({
      version: 1,
      models: {
        "qwen/text-embedding-v4": { device: "metal" },
      },
    }),
  );
  assert.throws(
    () => readGlobalConfig(configPath),
    (error) => {
      assert.match(error.context, /only supports local embedding models/);
      return true;
    },
  );

  await writeFile(
    configPath,
    JSON.stringify({
      version: 1,
      defaults: {
        llamaGpu: "metal",
      },
    }),
  );
  assert.throws(
    () => readGlobalConfig(configPath),
    (error) => {
      assert.match(error.context, /defaults\.llamaGpu is not supported/);
      return true;
    },
  );
});
