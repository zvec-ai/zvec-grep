import assert from "node:assert/strict";
import { mkdtemp, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  readGlobalConfig,
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
        llamaGpu: false,
      },
      providers: {
        qwen: {
          apiKey: "first-key",
        },
      },
    },
    configPath,
  );
  updateGlobalConfig(
    {
      defaults: {
        embeddingParallelism: 3,
      },
      providers: {
        qwen: {
          endpoint: "https://example.test/embeddings",
        },
      },
    },
    configPath,
  );

  assert.deepEqual(readGlobalConfig(configPath), {
    version: 1,
    defaults: {
      embedding: "qwen/text-embedding-v4",
      llamaGpu: false,
      embeddingParallelism: 3,
    },
    providers: {
      qwen: {
        apiKey: "first-key",
        endpoint: "https://example.test/embeddings",
      },
    },
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
});
