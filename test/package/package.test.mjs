import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdir, readFile, stat, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { createFakeEmbeddingServer } from "../helpers/fake-embedding.mjs";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

const execFileAsync = promisify(execFile);

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

test("npm package contains and exposes the supported public surface", async (t) => {
  const temporaryDirectory = await createTemporaryDirectory(
    t,
    "zvec-grep-package-",
  );
  const packDirectory = join(temporaryDirectory, "pack");
  const consumerDirectory = join(temporaryDirectory, "consumer");
  const npmCache = join(temporaryDirectory, "npm-cache");
  const npmEnvironment = {
    ...process.env,
    npm_config_cache: npmCache,
  };
  await mkdir(packDirectory, { recursive: true });
  await mkdir(consumerDirectory, { recursive: true });

  const packed = await runNpm(
    ["pack", "--json", "--ignore-scripts", "--pack-destination", packDirectory],
    { cwd: resolve("."), env: npmEnvironment, timeout: 120_000 },
  );
  const [metadata] = JSON.parse(packed.stdout);
  const paths = new Map(metadata.files.map((file) => [file.path, file]));
  for (const required of [
    "LICENSE",
    "README.md",
    "README_CN.md",
    "dist/index.js",
    "dist/index.d.ts",
    "dist/cli/index.js",
    "skills/zvec-grep/SKILL.md",
  ]) {
    assert.ok(paths.has(required), `package is missing ${required}`);
  }
  assert.equal(
    [...paths].some(
      ([path]) => path.startsWith("src/") || path.startsWith("test/"),
    ),
    false,
  );

  const tarball = join(packDirectory, metadata.filename);
  await writeFile(
    join(consumerDirectory, "package.json"),
    JSON.stringify({ private: true, type: "module" }),
  );
  await runNpm(["install", "--no-audit", "--no-fund", tarball], {
    cwd: consumerDirectory,
    env: npmEnvironment,
    timeout: 180_000,
  });

  const packageJson = JSON.parse(
    await readFile(
      join(
        consumerDirectory,
        "node_modules",
        "@zvec",
        "zvec-grep",
        "package.json",
      ),
      "utf8",
    ),
  );
  const cli = join(
    consumerDirectory,
    "node_modules",
    ".bin",
    process.platform === "win32" ? "zg.cmd" : "zg",
  );
  if (process.platform !== "win32") {
    const installedCli = await stat(
      join(
        consumerDirectory,
        "node_modules",
        "@zvec",
        "zvec-grep",
        "dist",
        "cli",
        "index.js",
      ),
    );
    assert.equal(installedCli.mode & 0o111, 0o111);
  }
  const version = await runExecutable(cli, ["version"], {
    cwd: consumerDirectory,
  });
  assert.equal(version.stdout.trim(), packageJson.version);
  const help = await runExecutable(cli, ["help"], {
    cwd: consumerDirectory,
  });
  assert.match(help.stdout, /Usage:/);

  await writeFile(
    join(consumerDirectory, "fixture.ts"),
    "export const PackageIndexedNeedle = 42;\n",
  );
  const rg = await runExecutable(
    cli,
    ["query", "--rg", "-g", "*.ts", "-t", "ts", "PackageIndexedNeedle", "."],
    { cwd: consumerDirectory },
  );
  assert.match(rg.stdout, /fixture\.ts/);

  const endpoint = await createFakeEmbeddingServer(t);
  const packageHome = join(temporaryDirectory, "package-home");
  const cliEnvironment = {
    ...process.env,
    HOME: packageHome,
    NO_COLOR: "1",
    ZVEC_GREP_HOME: packageHome,
  };
  await runExecutable(
    cli,
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
      "*.ts",
      "-t",
      "ts",
      ".",
    ],
    { cwd: consumerDirectory, env: cliEnvironment, timeout: 120_000 },
  );
  const indexed = await runExecutable(
    cli,
    [
      "query",
      "--fts",
      "PackageIndexedNeedle",
      "--limit",
      "5",
      "-g",
      "*.ts",
      "-t",
      "ts",
    ],
    { cwd: consumerDirectory, env: cliEnvironment, timeout: 120_000 },
  );
  assert.match(indexed.stdout, /PackageIndexedNeedle/);

  const imported = await execFileAsync(
    process.execPath,
    [
      "--input-type=module",
      "--eval",
      "import { createZvecGrep } from '@zvec/zvec-grep'; if (typeof createZvecGrep !== 'function') process.exit(1);",
    ],
    { cwd: consumerDirectory },
  );
  assert.equal(imported.stderr, "");

  const typeFixture = join(consumerDirectory, "consume.ts");
  await writeFile(
    typeFixture,
    "import { createZvecGrep } from '@zvec/zvec-grep';\nvoid createZvecGrep;\n",
  );
  await execFileAsync(
    process.execPath,
    [
      resolve("node_modules/typescript/bin/tsc"),
      "--noEmit",
      "--target",
      "ES2022",
      "--module",
      "NodeNext",
      "--moduleResolution",
      "NodeNext",
      typeFixture,
    ],
    { cwd: consumerDirectory },
  );
});
