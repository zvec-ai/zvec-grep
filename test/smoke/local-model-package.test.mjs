import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdir, mkdtemp, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import test from "node:test";

const execFileAsync = promisify(execFile);
const DEFAULT_MODEL = "local/potion-code-16m-v2";

function runNpm(args, options) {
  const npmExecPath = process.env.npm_execpath;
  if (npmExecPath) {
    return execFileAsync(process.execPath, [npmExecPath, ...args], options);
  }

  return execFileAsync("npm", args, options);
}

function runExecutable(file, args, options) {
  return execFileAsync(file, args, {
    ...options,
    shell: process.platform === "win32",
    windowsHide: true,
  });
}

async function resolvePackageTarball(root, temporaryDirectory, npmEnvironment) {
  const configured = process.env.ZVEC_GREP_PACKAGE_TARBALL;
  if (configured) {
    const candidate = resolve(configured);
    if ((await stat(candidate)).isFile()) {
      return candidate;
    }

    const tarballs = (await readdir(candidate))
      .filter((name) => name.endsWith(".tgz"))
      .sort();
    assert.equal(
      tarballs.length,
      1,
      `expected exactly one package tarball in ${candidate}`,
    );
    return join(candidate, tarballs[0]);
  }

  const packDirectory = join(temporaryDirectory, "pack");
  await mkdir(packDirectory, { recursive: true });
  const packed = await runNpm(
    ["pack", "--json", "--pack-destination", packDirectory],
    { cwd: root, env: npmEnvironment, timeout: 180_000 },
  );
  const [metadata] = JSON.parse(packed.stdout);
  return join(packDirectory, metadata.filename);
}

test("packed package runs a real local embedding model end to end", async (t) => {
  const root = resolve(".");
  const temporaryDirectory = await mkdtemp(
    join(tmpdir(), "zvec-grep-local-model-package-"),
  );
  t.after(() => rm(temporaryDirectory, { recursive: true, force: true }));

  const consumerDirectory = join(temporaryDirectory, "consumer");
  const modelCache = resolve(
    process.env.ZVEC_GREP_MODEL_CACHE ??
      join(temporaryDirectory, "model-cache"),
  );
  const modelReference =
    process.env.ZVEC_GREP_SMOKE_MODEL?.trim() || DEFAULT_MODEL;
  const npmEnvironment = {
    ...process.env,
    npm_config_registry: "https://registry.npmjs.org/",
  };

  await mkdir(consumerDirectory, { recursive: true });
  await writeFile(
    join(consumerDirectory, "package.json"),
    JSON.stringify({ private: true, type: "module" }),
  );
  const tarball = await resolvePackageTarball(
    root,
    temporaryDirectory,
    npmEnvironment,
  );
  const installArguments = ["install", "--no-audit", "--no-fund"];
  installArguments.push(tarball);
  await runNpm(installArguments, {
    cwd: consumerDirectory,
    env: npmEnvironment,
    timeout: 600_000,
  });

  const cli = join(
    consumerDirectory,
    "node_modules",
    ".bin",
    process.platform === "win32" ? "zg.cmd" : "zg",
  );
  const packageHome = join(temporaryDirectory, "home");
  const runtimeEnvironment = {
    ...process.env,
    HOME: packageHome,
    USERPROFILE: packageHome,
    NO_COLOR: "1",
    ZVEC_GREP_HOME: packageHome,
    ZVEC_GREP_MODEL_CACHE: modelCache,
  };

  const embeddingSmokeScript = join(consumerDirectory, "embedding-smoke.mjs");
  await writeFile(
    embeddingSmokeScript,
    [
      "import { createEmbeddingModel } from '@zvec/zvec-grep';",
      "const reference = process.env.ZVEC_GREP_SMOKE_MODEL;",
      "const model = createEmbeddingModel(reference, { modelCacheDir: process.env.ZVEC_GREP_MODEL_CACHE, device: 'cpu' });",
      "try {",
      "  const result = await model.embed([{ kind: 'text', text: 'CrossPlatformPotionNeedle validates package-local embedding.' }], { purpose: 'query' });",
      "  const vector = result.vectors[0];",
      "  if (!vector || vector.length !== model.info.dimension || vector.some((value) => !Number.isFinite(value))) throw new Error('invalid embedding vector');",
      "  const norm = Math.sqrt(vector.reduce((sum, value) => sum + value * value, 0));",
      "  if (!(norm > 0 && Number.isFinite(norm))) throw new Error('invalid embedding norm');",
      "  console.log('ZVEC_GREP_SMOKE_RESULT=' + JSON.stringify({ dimension: vector.length, norm }));",
      "} finally {",
      "  await model.dispose();",
      "}",
      "",
    ].join("\n"),
  );
  const directEmbedding = await execFileAsync(
    process.execPath,
    [embeddingSmokeScript],
    {
      cwd: consumerDirectory,
      env: {
        ...runtimeEnvironment,
        ZVEC_GREP_SMOKE_MODEL: modelReference,
      },
      timeout: 600_000,
    },
  );
  const resultLine = directEmbedding.stdout
    .split(/\r?\n/)
    .find((line) => line.startsWith("ZVEC_GREP_SMOKE_RESULT="));
  assert.ok(resultLine, "direct embedding did not report a result");
  const embeddingResult = JSON.parse(resultLine.split("=", 2)[1]);
  assert.ok(embeddingResult.dimension > 0);
  assert.ok(embeddingResult.norm > 0);

  await writeFile(
    join(consumerDirectory, "fixture.ts"),
    [
      "export function CrossPlatformPotionNeedle(token: string): boolean {",
      "  return token.startsWith('potion-');",
      "}",
      "",
    ].join("\n"),
  );
  const indexed = await runExecutable(
    cli,
    [
      "index",
      "--mode",
      "direct",
      "--embedding",
      modelReference,
      "--device",
      "cpu",
      "-g",
      "fixture.ts",
      "-t",
      "ts",
      ".",
    ],
    {
      cwd: consumerDirectory,
      env: runtimeEnvironment,
      timeout: 600_000,
    },
  );
  assert.match(indexed.stdout, /^Workspace index$/m);
  assert.match(indexed.stdout, /1 added/);

  const queried = await runExecutable(
    cli,
    [
      "query",
      "--mode",
      "direct",
      "--vector",
      "CrossPlatformPotionNeedle",
      "--limit",
      "5",
      "-g",
      "fixture.ts",
      "-t",
      "ts",
      ".",
    ],
    {
      cwd: consumerDirectory,
      env: runtimeEnvironment,
      timeout: 600_000,
    },
  );
  assert.match(queried.stdout, /CrossPlatformPotionNeedle/);
  assert.match(queried.stdout, /fixture\.ts/);

  const status = await runExecutable(cli, ["status", "--mode", "direct", "."], {
    cwd: consumerDirectory,
    env: runtimeEnvironment,
    timeout: 120_000,
  });
  assert.match(
    status.stdout,
    new RegExp(modelReference.replaceAll("/", "\\/")),
  );
});
