import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { access, mkdir, mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { parseArgs } from "../dist/cli/args.js";

const execFileAsync = promisify(execFile);
const cliPath = resolve("dist/cli/index.js");

test("config model set parses local runtime settings", () => {
  const parsed = parseArgs([
    "config",
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
      "config",
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
    [
      cliPath,
      "config",
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
      "config",
      "model",
      "set",
      "local/qwen3-embedding-0.6b",
      "--device",
      "cpu",
    ],
    { env, cwd: workspace },
  );

  const config = JSON.parse(
    await readFile(join(home, ".zvec-grep", "config.json"), "utf8"),
  );
  assert.deepEqual(config.models, {
    "local/embeddinggemma-300m": { device: "cpu" },
    "local/qwen3-embedding-0.6b": { device: "cpu" },
  });
  await assert.rejects(access(join(workspace, ".zvec-grep")), {
    code: "ENOENT",
  });
});

test("config model set rejects missing settings and remote models", async () => {
  await assert.rejects(
    execFileAsync(process.execPath, [
      cliPath,
      "config",
      "model",
      "set",
      "local/embeddinggemma-300m",
    ]),
    /requires --device/,
  );
  await assert.rejects(
    execFileAsync(process.execPath, [
      cliPath,
      "config",
      "model",
      "set",
      "qwen/text-embedding-v4",
      "--device",
      "cpu",
    ]),
    /only supports local embedding models/,
  );
  await assert.rejects(
    execFileAsync(process.execPath, [
      cliPath,
      "config",
      "model",
      "set",
      "local/unknown",
      "--device",
      "cpu",
    ]),
    /not in the zvec-grep catalog/,
  );
  for (const option of ["--gpu", "--no-gpu", "--llama-gpu"]) {
    assert.throws(
      () =>
        parseArgs([
          "config",
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
        "config",
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
});
