#!/usr/bin/env node

import {
  chmod,
  appendFile,
  copyFile,
  cp,
  mkdir,
  readdir,
  readFile,
  rename,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import {
  cargoPackageVersion,
  buildNative,
  existingNativeBuild,
  loadReleaseManifest,
  npmEnvironment,
  platformByTarget,
  run,
  selectPlatform,
  sha256File,
  stageRuntimeBundle,
  stageZvecRuntime,
  workspaceDir,
} from "./npm-package-core.mjs";
import {
  collectRelease,
  promoteRelease,
  publishRelease,
} from "./npm-release-registry.mjs";
import {
  stageCompliance,
  verifyPublicationInputs,
} from "./npm-release-compliance.mjs";
import {
  rollbackVersion,
  smokeArtifactRelease,
  smokeInstalledPackage,
  smokeRegistryRelease,
} from "./npm-release-smoke.mjs";

const scriptPath = fileURLToPath(import.meta.url);
const releaseRoot = join(workspaceDir, "dist", "npm", "release");
const stagingRoot = join(releaseRoot, "staging");
const tarballRoot = join(releaseRoot, "tarballs");
const cacheRoot = join(releaseRoot, "cache");

function parseOptions(args) {
  const options = {
    build: true,
    binary: undefined,
    bundleDir: undefined,
    cpuOnly: false,
    libDir: undefined,
    target: undefined,
    targets: undefined,
  };
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === "--no-build") {
      options.build = false;
      continue;
    }
    if (argument === "--cpu-only") {
      options.cpuOnly = true;
      continue;
    }
    const fields = new Map([
      ["--binary", "binary"],
      ["--bundle-dir", "bundleDir"],
      ["--lib-dir", "libDir"],
      ["--target", "target"],
      ["--targets", "targets"],
    ]);
    const field = fields.get(argument);
    if (field) {
      const value = args[index + 1];
      if (!value) {
        throw new Error(`${argument} requires a value`);
      }
      options[field] =
        field === "targets"
          ? value.split(",").map((target) => target.trim())
          : field === "target"
            ? value
            : resolve(value);
      index += 1;
      continue;
    }
    throw new Error(`unknown option: ${argument}`);
  }
  if (options.bundleDir && (options.binary || options.libDir)) {
    throw new Error("--bundle-dir cannot be combined with --binary or --lib-dir");
  }
  if (options.cpuOnly && options.bundleDir) {
    throw new Error("--cpu-only cannot be combined with --bundle-dir");
  }
  return options;
}

function currentPlatform(manifest) {
  const report = process.report?.getReport();
  return selectPlatform(
    manifest,
    process.platform,
    process.arch,
    report?.header ?? {},
  );
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
      files.push(...(await listFiles(root, path)));
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
    throw new Error(
      `npm pack did not report a tarball for ${packageDirectory}`,
    );
  }
  return join(tarballRoot, result[0].filename);
}

async function packPlatform(options) {
  const manifest = loadReleaseManifest();
  const current = currentPlatform(manifest);
  const entry = options.target
    ? platformByTarget(manifest, options.target)
    : current;
  if (options.bundleDir && options.build) {
    throw new Error("--bundle-dir requires --no-build");
  }
  if (options.build && entry.target !== current.target) {
    throw new Error("cross-platform packaging requires --no-build and explicit artifacts");
  }
  const build = options.build
    ? await buildNative(entry, options)
    : await existingNativeBuild(entry, options);
  const bundleDir = options.bundleDir ?? (options.binary || options.libDir ? undefined : build.bundleDir);
  const binary = bundleDir
    ? join(bundleDir, "bin", entry.binary)
    : options.binary ?? join(build.profileDir, entry.binary);
  await stat(binary).catch(() => {
    throw new Error(`missing native release binary: ${binary}`);
  });
  const version = cargoPackageVersion();
  const destination = join(stagingRoot, entry.target);
  await replaceDirectory(destination, async (temporary) => {
    const binDir = join(temporary, "bin");
    const libDir = join(temporary, "lib");
    await mkdir(binDir, { recursive: true });
    await mkdir(libDir, { recursive: true });
    if (bundleDir) {
      await stageRuntimeBundle(bundleDir, temporary, entry.os, entry.binary);
    } else {
      const packagedBinary = join(binDir, entry.binary);
      await copyFile(binary, packagedBinary);
      await stageZvecRuntime(options.libDir ?? dirname(binary), libDir, entry.os);
    }
    if (entry.os !== "win32") {
      await chmod(join(binDir, entry.binary), 0o755);
    }

    const checksums = await checksumManifest(temporary, entry.target);
    await stageCompliance(
      join(workspaceDir, "npm", "compliance", entry.target),
      temporary,
    );

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
      zvecGrep: {
        variant: bundleDir ? "cuda" : "cpu",
        accelerators: bundleDir ? ["cpu", "cuda"]
          : entry.os === "darwin" ? ["cpu", "metal"]
          : entry.os === "win32" ? ["cpu", "directml"] : ["cpu"],
      },
    };
    if (entry.libc) {
      packageJson.libc = [entry.libc];
    }
    await writeFile(
      join(temporary, "package.json"),
      `${JSON.stringify(packageJson, null, 2)}\n`,
    );
    await writeFile(
      join(temporary, "checksums.json"),
      `${JSON.stringify(checksums, null, 2)}\n`,
    );
    await writeFile(
      join(temporary, "README.md"),
      `# ${entry.package}\n\nNative ${bundleDir ? "CPU/CUDA" : "CPU"} zvec-grep payload for ${entry.target}. Install @zvec/zvec-grep instead.\n`,
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
    await writeFile(
      join(temporary, "package.json"),
      `${JSON.stringify(packageJson, null, 2)}\n`,
    );
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
  const packageNames = new Set(
    manifest.platforms.map((entry) => entry.package),
  );
  if (packageNames.size !== manifest.platforms.length) {
    throw new Error("npm platform package names must be unique");
  }
  console.log(
    `verified ${manifest.platforms.length} npm platform packages at version ${version}`,
  );
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
      `--allow-scripts=${meta.tarball}`,
      platform.tarball,
      meta.tarball,
    ],
    { env: npmEnvironment(cacheRoot) },
  );

  await smokeInstalledPackage({
    manifest: loadReleaseManifest(),
    entry: platform.entry,
    version: platform.version,
    prefix,
    fixtureRoot: join(releaseRoot, "smoke-workspace"),
  });
}

async function main() {
  const command = process.argv[2];
  const options = parseOptions(process.argv.slice(3));
  if (options.targets !== undefined && command !== "collect") {
    throw new Error("--targets is only supported by collect");
  }
  if (command === "build") {
    await buildNative(currentPlatform(loadReleaseManifest()), options);
    return;
  }
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
  if (command === "verify-publication") {
    await verifyPublicationInputs(
      loadReleaseManifest(),
      join(workspaceDir, "npm", "compliance"),
    );
    return;
  }
  if (command === "smoke-current") {
    await smokeCurrent(options);
    return;
  }
  if (command === "smoke-artifacts") {
    await smokeArtifactRelease({
      manifest: loadReleaseManifest(),
      release: JSON.parse(await readFile(join(tarballRoot, "release.json"), "utf8")),
      tarballRoot,
      prefixRoot: join(releaseRoot, "artifact-smoke"),
    });
    return;
  }
  if (command === "collect") {
    await packMeta();
    await collectRelease({
      manifest: loadReleaseManifest(),
      version: cargoPackageVersion(),
      tarballRoot,
      targets: options.targets,
    });
    return;
  }
  if (command === "publish") {
    await publishRelease({ manifest: loadReleaseManifest(), tarballRoot });
    return;
  }
  if (command === "promote") {
    await promoteRelease({ manifest: loadReleaseManifest(), tarballRoot });
    return;
  }
  if (command === "rollback-version") {
    const baseline = await rollbackVersion(
      loadReleaseManifest(),
      cargoPackageVersion(),
    );
    if (process.env.GITHUB_OUTPUT)
      await appendFile(process.env.GITHUB_OUTPUT, `baseline=${baseline}\n`);
    console.log(baseline);
    return;
  }
  if (command === "smoke-registry") {
    await smokeRegistryRelease({
      manifest: loadReleaseManifest(),
      release: JSON.parse(
        await readFile(join(tarballRoot, "release.json"), "utf8"),
      ),
      baseline: process.env.NPM_ROLLBACK_VERSION,
      prefixRoot: join(releaseRoot, "registry-smoke"),
    });
    return;
  }
  throw new Error(
    "usage: npm-release.mjs <build|verify|verify-publication|pack-platform|pack-meta|pack-current|smoke-current|smoke-artifacts|collect|publish|promote|rollback-version|smoke-registry> [--target TARGET] [--targets TARGET,... (collect only)] [--binary PATH] [--lib-dir PATH] [--bundle-dir PATH] [--cpu-only] [--no-build]",
  );
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  main().catch((error) => {
    console.error(`npm-release: ${error.message}`);
    process.exitCode = 1;
  });
}
