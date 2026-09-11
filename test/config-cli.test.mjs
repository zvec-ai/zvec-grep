import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { access, mkdir, mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { parseArgs } from "../dist/cli/args.js";
import { resolveAuthorizationSchema } from "../dist/cli/auth.js";

const execFileAsync = promisify(execFile);
const cliPath = resolve("dist/cli/index.js");

test("no-prefetch is accepted only for indexing", () => {
  for (const mode of ["direct", "server"]) {
    assert.equal(
      parseArgs(["--index", "--mode", mode, "--no-prefetch", "."]).options
        .noPrefetch,
      true,
    );
  }
  for (const args of [["query"], ["--status"], ["--index", "--drop"]]) {
    assert.throws(() => parseArgs([...args, "--no-prefetch"]), /--no-prefetch/);
  }
});

test("catalog references resolve to served model identities", () => {
  assert.deepEqual(resolveAuthorizationSchema("dgx/qwen3-embedding-0.6b"), {
    provider: "dgx",
    model: "qwen3-embedding:0.6b",
  });
});

test("config model set parses local runtime settings", () => {
  const parsed = parseArgs([
    "--config",
    "model",
    "set",
    "local/embeddinggemma-300m",
    "--device",
    "metal",
  ]);
  assert.equal(parsed.options.configAction, "model-set");
  assert.equal(parsed.options.device, "metal");
  assert.deepEqual(parsed.positionals, ["local/embeddinggemma-300m"]);
});

test("config provider and default model settings parse", () => {
  const provider = parseArgs([
    "--config",
    "provider",
    "set",
    "qwen",
    "--api-key",
    "secret",
  ]);
  assert.equal(provider.options.configAction, "provider-set");
  assert.equal(provider.options.apiKey, "secret");

  const unauthenticatedProvider = parseArgs([
    "--config",
    "provider",
    "set",
    "dgx",
    "--no-auth",
  ]);
  assert.equal(unauthenticatedProvider.options.configAction, "provider-set");
  assert.equal(unauthenticatedProvider.options.providerNoAuth, true);

  const model = parseArgs([
    "--config",
    "model",
    "set",
    "qwen/text-embedding-v4",
    "--endpoint",
    "https://example.test/embeddings",
    "--default",
  ]);
  assert.equal(model.options.endpoint, "https://example.test/embeddings");
  assert.equal(model.options.defaultModel, true);
});

test("config model set persists independent local model settings", async (t) => {
  const home = await mkdtemp(join(tmpdir(), "zvec-grep-config-cli-"));
  const workspace = join(home, "workspace");
  await mkdir(workspace);
  t.after(async () => {
    await rm(home, { recursive: true, force: true });
  });
  const env = { ...process.env, HOME: home, USERPROFILE: home };

  await execFileAsync(
    process.execPath,
    [
      cliPath,
      "--config",
      "model",
      "set",
      "local/embeddinggemma-300m",
      "--device",
      "metal",
    ],
    { env, cwd: workspace },
  );
  await execFileAsync(
    process.execPath,
    [cliPath, "config", "provider", "set", "dgx", "--no-auth"],
    { env, cwd: workspace },
  );
  await execFileAsync(
    process.execPath,
    [
      cliPath,
      "--config",
      "model",
      "set",
      "local/embeddinggemma-300m",
      "--device",
      "cpu",
    ],
    { env, cwd: workspace },
  );
  await execFileAsync(
    process.execPath,
    [
      cliPath,
      "--config",
      "model",
      "set",
      "local/qwen3-embedding-0.6b",
      "--device",
      "cpu",
    ],
    { env, cwd: workspace },
  );
  await execFileAsync(
    process.execPath,
    [
      cliPath,
      "--config",
      "provider",
      "set",
      "qwen",
      "--api-key",
      "provider-key",
    ],
    { env, cwd: workspace },
  );
  await execFileAsync(
    process.execPath,
    [
      cliPath,
      "--config",
      "model",
      "set",
      "qwen/text-embedding-v4",
      "--endpoint",
      "https://example.test/embeddings",
      "--default",
    ],
    { env, cwd: workspace },
  );

  const config = JSON.parse(
    await readFile(join(home, ".zvec-grep", "config.json"), "utf8"),
  );
  assert.deepEqual(config.models, {
    "local/embeddinggemma-300m": { device: "cpu" },
    "local/qwen3-embedding-0.6b": { device: "cpu" },
    "qwen/text-embedding-v4": {
      endpoint: "https://example.test/embeddings",
    },
  });
  assert.deepEqual(config.providers, {
    qwen: { apiKey: "provider-key" },
    dgx: { auth: "none" },
  });
  assert.equal(config.defaults.embedding, "qwen/text-embedding-v4");
  assert.equal(config.version, 1);
  await assert.rejects(access(join(workspace, ".zvec-grep")), {
    code: "ENOENT",
  });
});

test("config model set rejects missing and incompatible settings", async () => {
  await assert.rejects(
    execFileAsync(process.execPath, [
      cliPath,
      "--config",
      "model",
      "set",
      "local/embeddinggemma-300m",
    ]),
    /requires --endpoint, --device, or --default/,
  );
  await assert.rejects(
    execFileAsync(process.execPath, [
      cliPath,
      "--config",
      "provider",
      "set",
      "qwen",
      "--no-auth",
    ]),
    /requires an API key and does not support --no-auth/,
  );
  await assert.rejects(
    execFileAsync(process.execPath, [
      cliPath,
      "--config",
      "provider",
      "set",
      "dgx",
      "--api-key",
      "secret",
      "--no-auth",
    ]),
    /either --api-key or --no-auth, not both/,
  );
  await assert.rejects(
    execFileAsync(process.execPath, [
      cliPath,
      "--config",
      "model",
      "set",
      "qwen/text-embedding-v4",
      "--device",
      "cpu",
    ]),
    /only supported for local embedding models/,
  );
  await assert.rejects(
    execFileAsync(process.execPath, [
      cliPath,
      "--config",
      "model",
      "set",
      "local/unknown",
      "--device",
      "cpu",
    ]),
    (error) => {
      assert.match(error.stderr, /Unsupported embedding model/);
      assert.match(error.stderr, /zg --help models/);
      return true;
    },
  );
  await assert.rejects(
    execFileAsync(process.execPath, [
      cliPath,
      "--config",
      "model",
      "set",
      "local/embeddinggemma-300m",
      "--endpoint",
      "https://example.test/embeddings",
    ]),
    /only supported for remote embedding models/,
  );
  await assert.rejects(
    execFileAsync(process.execPath, [
      cliPath,
      "--config",
      "provider",
      "set",
      "unknown",
      "--api-key",
      "secret",
    ]),
    (error) => {
      assert.match(error.stderr, /Unsupported remote embedding provider/);
      assert.match(
        error.stderr,
        /Supported remote embedding providers:\s+qwen\s+dgx/,
      );
      return true;
    },
  );
  for (const option of ["--gpu", "--no-gpu", "--llama-gpu"]) {
    assert.throws(
      () =>
        parseArgs([
          "--config",
          "model",
          "set",
          "local/embeddinggemma-300m",
          option,
        ]),
      /Unknown option/,
    );
  }
  assert.throws(
    () =>
      parseArgs([
        "--config",
        "model",
        "set",
        "local/embeddinggemma-300m",
        "--device",
        "cpu",
        "--api-key",
        "secret",
      ]),
    /does not accept --api-key/,
  );
  assert.throws(
    () =>
      parseArgs([
        "config",
        "model",
        "set",
        "local/embeddinggemma-300m",
        "--no-auth",
      ]),
    /does not accept --no-auth/,
  );
});
