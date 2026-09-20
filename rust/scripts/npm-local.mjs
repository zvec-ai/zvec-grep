#!/usr/bin/env node

import {
  chmod,
  copyFile,
  link,
  mkdir,
  rename,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import {
  cargoPackageVersion,
  loadReleaseManifest,
  npmEnvironment as sharedNpmEnvironment,
  run,
  selectPlatform,
  sha256File,
  stageZvecRuntime,
  workspaceDir,
} from "./npm-package-core.mjs";

const scriptPath = fileURLToPath(import.meta.url);
const outputDir = join(workspaceDir, "dist", "npm");
const localPackageDir = join(outputDir, "zvec-grep-local");
const packedPackageDir = join(outputDir, "zvec-grep-package");

function npmEnvironment() {
  return sharedNpmEnvironment(join(outputDir, "cache"));
}

function currentPlatform() {
  const report = process.report?.getReport();
  const entry = selectPlatform(
    loadReleaseManifest(),
    process.platform,
    process.arch,
    report?.header ?? {},
  );
  return {
    platform: entry.os,
    arch: entry.cpu,
    libc: entry.libc,
    target: entry.target,
    binary: entry.binary,
  };
}

function parseOptions(args) {
  const options = { build: true, prefix: undefined };
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === "--no-build") {
      options.build = false;
      continue;
    }
    if (argument === "--prefix") {
      const prefix = args[index + 1];
      if (!prefix) {
        throw new Error("--prefix requires a path");
      }
      options.prefix = resolve(prefix);
      index += 1;
      continue;
    }
    throw new Error(`unknown option: ${argument}`);
  }
  return options;
}

async function stagePackage(
  { build },
  { destination = packedPackageDir, linkToBuild = false } = {},
) {
  const descriptor = currentPlatform();
  const version = cargoPackageVersion();
  if (build) {
    run("cargo", ["build", "--locked", "--release", "--package", "zg"]);
  }

  const sourceName = descriptor.binary;
  const sourceBinary = join(workspaceDir, "target", "release", sourceName);
  try {
    await stat(sourceBinary);
  } catch {
    throw new Error(
      `missing ${sourceBinary}; run without --no-build or build the zg release binary first`,
    );
  }

  const temporaryDir = `${destination}.${process.pid}.tmp`;
  await mkdir(outputDir, { recursive: true });
  await rm(temporaryDir, { recursive: true, force: true });
  await mkdir(join(temporaryDir, "bin"), { recursive: true });

  const packagedBinary = join(temporaryDir, "bin", sourceName);
  if (linkToBuild && process.platform !== "win32") {
    await symlink(sourceBinary, packagedBinary);
  } else if (linkToBuild) {
    await link(sourceBinary, packagedBinary);
  } else {
    await copyFile(sourceBinary, packagedBinary);
  }
  if (process.platform !== "win32") {
    await chmod(packagedBinary, 0o755);
  }

  const runtimeFiles = await stageZvecRuntime(
    join(workspaceDir, "target", "release"),
    join(temporaryDir, "bin"),
    descriptor.platform,
  );

  const binaryStat = await stat(packagedBinary);
  const packageJson = {
    name: "@zvec/zvec-grep",
    version,
    description: `Local ${descriptor.target} build of the zvec-grep Rust CLI`,
    license: "Apache-2.0",
    repository: "https://github.com/zvec-ai/zvec-grep",
    os: [descriptor.platform],
    cpu: [descriptor.arch],
    bin: { zg: `bin/${sourceName}` },
    files: ["bin", "checksums.json", "README.md"],
  };
  if (descriptor.libc) {
    packageJson.libc = [descriptor.libc];
  }

  const checksums = {
    schemaVersion: 1,
    target: descriptor.target,
    files: {
      [`bin/${sourceName}`]: {
        bytes: binaryStat.size,
        sha256: await sha256File(packagedBinary),
      },
    },
  };
  for (const file of runtimeFiles) {
    const path = join(temporaryDir, file);
    checksums.files[file] = {
      bytes: (await stat(path)).size,
      sha256: await sha256File(path),
    };
  }
  const packageReadme = [
    "# Local zvec-grep npm package",
    "",
    `This tarball contains zvec-grep ${version} for ${descriptor.target}.`,
    "It was built locally and is not a registry release.",
    "",
  ].join("\n");

  await writeFile(
    join(temporaryDir, "package.json"),
    `${JSON.stringify(packageJson, null, 2)}\n`,
  );
  await writeFile(
    join(temporaryDir, "checksums.json"),
    `${JSON.stringify(checksums, null, 2)}\n`,
  );
  await writeFile(join(temporaryDir, "README.md"), packageReadme);

  await rm(destination, { recursive: true, force: true });
  await rename(temporaryDir, destination);
  return { descriptor, version };
}

async function pack(options) {
  const staged = await stagePackage(options);
  const stdout = run(
    "npm",
    ["pack", "--json", "--pack-destination", outputDir],
    {
      capture: true,
      cwd: packedPackageDir,
      env: npmEnvironment(),
    },
  );
  const result = JSON.parse(stdout);
  const filename = result[0]?.filename;
  if (!filename) {
    throw new Error("npm pack did not report an output filename");
  }
  const tarball = join(outputDir, filename);
  console.log(`packed ${staged.descriptor.target}: ${tarball}`);
  return { ...staged, tarball };
}

async function install(options) {
  const staged = await stagePackage(options, {
    destination: localPackageDir,
    linkToBuild: true,
  });
  const args = [
    "install",
    "--global",
    "--install-links=false",
    "--no-audit",
    "--no-fund",
  ];
  if (options.prefix) {
    args.push("--prefix", options.prefix);
  }
  args.push(localPackageDir);
  run("npm", args, { env: npmEnvironment() });
  console.log(
    options.prefix
      ? `installed zg under ${options.prefix}`
      : "installed zg in the active npm global prefix",
  );
  return staged;
}

function installedBinary(prefix) {
  if (process.platform === "win32") {
    return join(prefix, "zg.cmd");
  }
  return join(prefix, "bin", "zg");
}

function runInstalled(binary, args, mode) {
  const result = spawnSync(binary, args, {
    cwd: workspaceDir,
    encoding: "utf8",
    env: { ...process.env, ZVEC_GREP_MODE: mode },
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(
      `${binary} ${args.join(" ")} failed in ${mode} mode:\n${result.stderr}`,
    );
  }
  return result.stdout;
}

async function smoke(options) {
  const prefix = options.prefix ?? join(outputDir, "smoke-prefix");
  await rm(prefix, { recursive: true, force: true });
  const packed = await install({ ...options, prefix });
  const binary = installedBinary(prefix);
  const installedVersion = runInstalled(binary, ["--version"], "direct").trim();
  if (installedVersion !== packed.version) {
    throw new Error(
      `installed zg version ${installedVersion} did not match package ${packed.version}`,
    );
  }

  await pack({ ...options, build: false });
  const versionAfterPack = runInstalled(binary, ["--version"], "direct").trim();
  if (versionAfterPack !== installedVersion) {
    throw new Error("packing replaced or invalidated the linked local zg binary");
  }

  const query = ["query", "--rg", "-F", "zvec-grep Rust rewrite", "README.md"];
  const directOutput = runInstalled(binary, query, "direct");
  const serverOutput = runInstalled(binary, query, "server");
  if (directOutput !== serverOutput) {
    throw new Error("installed zg produced different managed-rg output by mode");
  }
  console.log(`smoke passed: zg ${installedVersion} (${packed.descriptor.target})`);
}

async function main() {
  const command = process.argv[2];
  const options = parseOptions(process.argv.slice(3));
  if (command === "pack") {
    await pack(options);
    return;
  }
  if (command === "install") {
    await install(options);
    return;
  }
  if (command === "smoke") {
    await smoke(options);
    return;
  }
  throw new Error("usage: npm-local.mjs <pack|install|smoke> [--no-build] [--prefix PATH]");
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  main().catch((error) => {
    console.error(`npm-local: ${error.message}`);
    process.exitCode = 1;
  });
}
