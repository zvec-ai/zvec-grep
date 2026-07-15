import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

const execFileAsync = promisify(execFile);

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

  const packed = await execFileAsync(
    "npm",
    ["pack", "--json", "--pack-destination", packDirectory],
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
  assert.equal(paths.get("dist/cli/index.js").mode & 0o111, 0o111);
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
  await execFileAsync(
    "npm",
    ["install", "--ignore-scripts", "--no-audit", "--no-fund", tarball],
    { cwd: consumerDirectory, env: npmEnvironment, timeout: 180_000 },
  );

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
  const version = await execFileAsync(cli, ["--version"], {
    cwd: consumerDirectory,
  });
  assert.equal(version.stdout.trim(), packageJson.version);
  const help = await execFileAsync(cli, ["--help"], { cwd: consumerDirectory });
  assert.match(help.stdout, /Usage:/);

  await writeFile(join(consumerDirectory, "fixture.txt"), "PackageNeedle\n");
  const rg = await execFileAsync(cli, ["--rg", "PackageNeedle", "."], {
    cwd: consumerDirectory,
  });
  assert.match(rg.stdout, /fixture\.txt/);

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
