#!/usr/bin/env node

import {
  chmod,
  copyFile,
  cp,
  mkdir,
  readdir,
  rename,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import {
  cargoPackageVersion,
  loadReleaseManifest,
  npmEnvironment,
  platformByTarget,
  run,
  selectPlatform,
  sha256File,
  stageZvecRuntime,
  workspaceDir,
} from "./npm-package-core.mjs";

const scriptPath = fileURLToPath(import.meta.url);
const releaseRoot = join(workspaceDir, "dist", "npm", "release");
const stagingRoot = join(releaseRoot, "staging");
const tarballRoot = join(releaseRoot, "tarballs");
const cacheRoot = join(releaseRoot, "cache");

function parseOptions(args) {
  const options = {
    build: true,
    binary: undefined,
    libDir: undefined,
    target: undefined,
  };
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === "--no-build") {
      options.build = false;
      continue;
    }
    const fields = new Map([
      ["--binary", "binary"],
      ["--lib-dir", "libDir"],
      ["--target", "target"],
    ]);
    const field = fields.get(argument);
    if (field) {
      const value = args[index + 1];
      if (!value) {
        throw new Error(`${argument} requires a value`);
      }
      options[field] = field === "target" ? value : resolve(value);
      index += 1;
      continue;
    }
    throw new Error(`unknown option: ${argument}`);
  }
  return options;
}

function currentPlatform(manifest) {
  const report = process.report?.getReport();
  return selectPlatform(manifest, process.platform, process.arch, report?.header ?? {});
}

async function replaceDirectory(destination, populate) {
  const temporary = `${destination}.${process.pid}.tmp`;
  await rm(temporary, { recursive: true, force: true });
  await mkdir(temporary, { recursive: true });
  try {
    await populate(temporary);
    await rm(destination, { recursive: true, force: true });
    await mkdir(dirname(destination), { recursive: true });
    await rename(temporary, destination);
  } catch (error) {
    await rm(temporary, { recursive: true, force: true });
    throw error;
  }
}

async function listFiles(root, directory = root) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      files.push(...await listFiles(root, path));
    } else {
      files.push(relative(root, path).split(sep).join("/"));
    }
  }
  return files.sort();
}

async function checksumManifest(root, target) {
  const files = {};
  for (const payloadPath of await listFiles(root)) {
    const path = join(root, ...payloadPath.split("/"));
    const metadata = await stat(path);
    files[payloadPath] = {
      bytes: metadata.size,
      sha256: await sha256File(path),
    };
  }
  return { schemaVersion: 1, target, files };
}

async function packDirectory(packageDirectory) {
  await mkdir(tarballRoot, { recursive: true });
  const stdout = run(
    "npm",
    ["pack", "--json", "--pack-destination", tarballRoot],
    {
      capture: true,
      cwd: packageDirectory,
      env: npmEnvironment(cacheRoot),
    },
  );
  const result = JSON.parse(stdout);
  if (!result[0]?.filename) {
    throw new Error(`npm pack did not report a tarball for ${packageDirectory}`);
  }
  return join(tarballRoot, result[0].filename);
}

async function packPlatform(options) {
  const manifest = loadReleaseManifest();
  const current = currentPlatform(manifest);
  const entry = options.target ? platformByTarget(manifest, options.target) : current;
  if (options.build) {
    if (entry.target !== current.target) {
      throw new Error("cross-platform packaging requires --no-build and --binary");
    }
    run("cargo", ["build", "--locked", "--release", "--package", "zg"]);
  }

  const binary = options.binary ?? join(workspaceDir, "target", "release", entry.binary);
  await stat(binary).catch(() => {
    throw new Error(`missing native release binary: ${binary}`);
  });
  const version = cargoPackageVersion();
  const destination = join(stagingRoot, entry.target);
  await replaceDirectory(destination, async (temporary) => {
    const binDir = join(temporary, "bin");
    await mkdir(binDir, { recursive: true });
    const packagedBinary = join(binDir, entry.binary);
    await copyFile(binary, packagedBinary);
    if (entry.os !== "win32") {
      await chmod(packagedBinary, 0o755);
    }
    await stageZvecRuntime(options.libDir ?? dirname(binary), binDir, entry.os);
    if (options.libDir) {
      await cp(options.libDir, join(temporary, "lib"), {
        recursive: true,
        dereference: true,
      });
    }

    const checksums = await checksumManifest(temporary, entry.target);
    const packageJson = {
      name: entry.package,
      version,
      description: `Native ${entry.target} distribution of zvec-grep`,
      license: "Apache-2.0",
      repository: "https://github.com/zvec-ai/zvec-grep",
      os: [entry.os],
      cpu: [entry.cpu],
      files: ["bin", "lib", "checksums.json", "README.md", "LICENSES", "SBOM.spdx.json"],
      publishConfig: { access: "public", provenance: true },
    };
    if (entry.libc) {
      packageJson.libc = [entry.libc];
    }
    await writeFile(join(temporary, "package.json"), `${JSON.stringify(packageJson, null, 2)}\n`);
    await writeFile(join(temporary, "checksums.json"), `${JSON.stringify(checksums, null, 2)}\n`);
    await writeFile(
      join(temporary, "README.md"),
      `# ${entry.package}\n\nNative zvec-grep payload for ${entry.target}. Install @zvec/zvec-grep instead.\n`,
    );
  });
  const tarball = await packDirectory(destination);
  console.log(`packed ${entry.package}@${version}: ${tarball}`);
  return { entry, version, destination, tarball };
}

async function packMeta() {
  const manifest = loadReleaseManifest();
  const version = cargoPackageVersion();
  const destination = join(stagingRoot, "meta");
  await replaceDirectory(destination, async (temporary) => {
    await cp(join(workspaceDir, "npm", "meta"), temporary, {
      recursive: true,
      dereference: true,
    });
    await chmod(join(temporary, "bin", "zg.exe"), 0o755);
    const optionalDependencies = Object.fromEntries(
      manifest.platforms.map((entry) => [entry.package, version]),
    );
    const packageJson = {
      name: manifest.metaPackage,
      version,
      description: "Native semantic and lexical code search CLI",
      license: "Apache-2.0",
      repository: "https://github.com/zvec-ai/zvec-grep",
      bin: { zg: "bin/zg.exe" },
      files: ["bin", "install.cjs", "platforms.json", "README.md"],
      scripts: { postinstall: "node install.cjs" },
      engines: { node: manifest.nodeEngine },
      optionalDependencies,
      publishConfig: { access: "public", provenance: true },
    };
    await writeFile(join(temporary, "package.json"), `${JSON.stringify(packageJson, null, 2)}\n`);
    await writeFile(
      join(temporary, "platforms.json"),
      `${JSON.stringify(manifest.platforms, null, 2)}\n`,
    );
  });
  const tarball = await packDirectory(destination);
  console.log(`packed ${manifest.metaPackage}@${version}: ${tarball}`);
  return { version, destination, tarball };
}

async function verifyManifest() {
  const manifest = loadReleaseManifest();
  const version = cargoPackageVersion();
  const packageNames = new Set(manifest.platforms.map((entry) => entry.package));
  if (packageNames.size !== manifest.platforms.length) {
    throw new Error("npm platform package names must be unique");
  }
  console.log(`verified ${manifest.platforms.length} npm platform packages at version ${version}`);
}

function installedPackageRoot(prefix, packageName) {
  const modules = process.platform === "win32"
    ? join(prefix, "node_modules")
    : join(prefix, "lib", "node_modules");
  return join(modules, ...packageName.split("/"));
}

async function smokeCurrent(options) {
  const platform = await packPlatform(options);
  const meta = await packMeta();
  const prefix = join(releaseRoot, "smoke-prefix");
  await rm(prefix, { recursive: true, force: true });
  run(
    "npm",
    [
      "install",
      "--global",
      "--prefix",
      prefix,
      "--no-audit",
      "--no-fund",
      `--allow-scripts=${loadReleaseManifest().metaPackage}`,
      platform.tarball,
      meta.tarball,
    ],
    { env: npmEnvironment(cacheRoot) },
  );

  const installedMeta = installedPackageRoot(prefix, loadReleaseManifest().metaPackage);
  const installedPlatform = installedPackageRoot(prefix, platform.entry.package);
  const materializedDigest = await sha256File(join(installedMeta, "bin", "zg.exe"));
  const platformDigest = await sha256File(
    join(installedPlatform, "bin", platform.entry.binary),
  );
  if (materializedDigest !== platformDigest) {
    throw new Error("materialized meta binary does not match the platform package");
  }
  console.log(`release smoke passed: ${platform.entry.target} payload materialized and verified`);
}

async function main() {
  const command = process.argv[2];
  const options = parseOptions(process.argv.slice(3));
  if (command === "pack-platform") {
    await packPlatform(options);
    return;
  }
  if (command === "pack-meta") {
    await packMeta();
    return;
  }
  if (command === "pack-current") {
    await packPlatform(options);
    await packMeta();
    return;
  }
  if (command === "verify") {
    await verifyManifest();
    return;
  }
  if (command === "smoke-current") {
    await smokeCurrent(options);
    return;
  }
  throw new Error(
    "usage: npm-release.mjs <verify|pack-platform|pack-meta|pack-current|smoke-current> [--target TARGET] [--binary PATH] [--lib-dir PATH] [--no-build]",
  );
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  main().catch((error) => {
    console.error(`npm-release: ${error.message}`);
    process.exitCode = 1;
  });
}
