#!/usr/bin/env node

import { chmod, copyFile, cp, lstat, mkdir, mkdtemp, readFile, readdir, rename, rm, stat, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { materialize } from "../npm/meta/install.cjs";
import { loadReleaseManifest, run, sha256File, workspaceDir } from "./npm-package-core.mjs";
import { serializeStandaloneManifest } from "./standalone-manifest.mjs";

const scriptPath = fileURLToPath(import.meta.url);
const versionPattern = /^[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?$/;

async function copyOptional(source, destination) {
  if (await stat(source).catch(() => undefined)) await cp(source, destination, { recursive: true });
}

export async function payloadFiles(root, directory = root) {
  const files = {};
  for (const entry of (await readdir(directory, { withFileTypes: true })).sort((a, b) => a.name.localeCompare(b.name))) {
    const path = join(directory, entry.name);
    const metadata = await lstat(path);
    if (metadata.isDirectory()) Object.assign(files, await payloadFiles(root, path));
    else if (metadata.isFile()) {
      const relative = path.slice(root.length + 1).replaceAll("\\", "/");
      files[relative] = { bytes: metadata.size, sha256: await sha256File(path) };
    } else throw new Error(`standalone payload contains a non-regular entry: ${path}`);
  }
  return files;
}

function extractPackage(archive, destination) {
  const paths = run("tar", ["-tzf", archive], { capture: true }).trim().split(/\r?\n/);
  if (paths.some((path) => !/^package(?:\/|$)/.test(path) || path.includes("\\") ||
      path.split("/").some((part) => part === ".." || part === "."))) throw new Error("unsafe package archive path");
  const types = run("tar", ["-tvzf", archive], { capture: true }).trim().split(/\r?\n/);
  if (types.some((line) => !/^[d-]/.test(line))) throw new Error("package archives may contain only files and directories");
  run("tar", ["-xzf", archive, "-C", destination]);
}

export async function collectStandaloneRelease({ manifest = loadReleaseManifest(), release, tarballRoot, outputDir }) {
  if (release.schemaVersion !== 1 || !versionPattern.test(release.version) || !/^[a-f0-9]{40}$/.test(release.commit) || !Array.isArray(release.targets) ||
      !release.targets.length || new Set(release.targets).size !== release.targets.length ||
      release.targets.some((target) => !manifest.platforms.some((entry) => entry.target === target))) {
    throw new Error("invalid collected release identity");
  }
  tarballRoot = resolve(tarballRoot);
  outputDir = resolve(outputDir);
  if (outputDir === tarballRoot) throw new Error("standalone output must be separate from npm input");
  await mkdir(outputDir, { recursive: true });
  const work = await mkdtemp(join(outputDir, ".staging-"));
  const inventory = { schemaVersion: 1, version: release.version, commit: release.commit,
    tag: `v${release.version}`, partial: release.targets.length !== manifest.platforms.length, assets: [] };
  try {
    for (const target of release.targets) {
      const entry = manifest.platforms.find((entry) => entry.target === target);
      const base = release.packages.find((pkg) => pkg.name === entry.package);
      if (!base) throw new Error(`missing base package: ${target}`);
      const source = join(work, target, "source");
      await mkdir(source, { recursive: true });
      for (const pkg of release.packages.filter((pkg) => pkg.target === target)) {
        if (pkg.version !== release.version || !/^[A-Za-z0-9._-]+\.tgz$/.test(pkg.file) ||
            await sha256File(join(tarballRoot, pkg.file)) !== pkg.sha256) throw new Error(`collected package checksum/identity mismatch: ${pkg.file}`);
      }
      extractPackage(join(tarballRoot, base.file), source);
      for (const accelerator of entry.components?.length ? ["cpu", "cuda"] : ["cpu"]) {
        const staging = join(work, target, accelerator);
        const payload = join(staging, "zg");
        await mkdir(payload, { recursive: true });
        materialize({ metaRoot: payload, platformRoot: join(source, "package"), entry, expectedVersion: release.version,
          config: { mode: accelerator, directory: tarballRoot, offline: true } });
        if (entry.os === "win32") {
          // The verified npm runtime carries the standalone launcher without adding another npm command.
          await rename(join(payload, "lib", "zg-launcher.exe"), join(payload, "bin", "zg-launcher.exe"));
        }
        // npm uses zg.exe on every OS; standalone preserves the platform's native name.
        if (entry.binary !== "zg.exe") await rename(join(payload, "bin", "zg.exe"), join(payload, "bin", entry.binary));
        if (entry.os !== "win32") await chmod(join(payload, "bin", entry.binary), 0o755);
        await rm(join(payload, "native-install.json"), { force: true });
        for (const name of ["LICENSE", "LICENSES", "SBOM.spdx.json"]) {
          await copyOptional(join(source, "package", name), join(payload, name));
        }
        if (accelerator === "cuda") {
          for (const component of entry.components) {
            const pkg = release.packages.find((pkg) => pkg.name === component.package);
            if (!pkg) throw new Error(`missing CUDA component: ${component.package}`);
            const componentRoot = join(work, "notices", component.package.replaceAll("/", "-"));
            await mkdir(componentRoot, { recursive: true });
            extractPackage(join(tarballRoot, pkg.file), componentRoot);
            for (const name of ["LICENSE", "LICENSES", "UPSTREAM_NOTICES", "SBOM.spdx.json"]) {
              await copyOptional(join(componentRoot, "package", name), join(payload, "licenses", component.package.replaceAll("/", "-"), name));
            }
          }
        }
        const files = await payloadFiles(payload);
        await writeFile(join(payload, "install-manifest.json"), `${JSON.stringify({
          schemaVersion: 1, version: release.version, commit: release.commit, target, accelerator,
          components: accelerator === "cuda" ? entry.components.map((component) => component.package) : [],
          requirements: { ...(entry.libc ? { libc: "glibc", minLibc: "2.28" } : {}),
            ...(target === "darwin-x64" ? { minOs: "14.0" } : {}),
            ...(accelerator === "cuda" ? { hostLibraries: ["NVIDIA driver", "CUDA runtime/cuBLAS 13.1 or 12.4 (GGUF)", "CUDA 12 runtime/cuBLAS and cuDNN 9 (ONNX)"] } : {}) },
          files,
        }, null, 2)}\n`);
        const file = `zvec-grep-${release.version}-${target}-${accelerator}.${entry.os === "win32" ? "zip" : "tar.gz"}`;
        const archive = join(outputDir, file);
        await rm(archive, { force: true });
        if (entry.os === "win32" && process.platform === "win32") run("tar", ["-a", "-cf", archive, "-C", staging, "zg"]);
        else if (entry.os === "win32") run("zip", ["-q", "-r", archive, "zg"], { cwd: staging });
        else run("tar", ["-czf", archive, "-C", staging, "zg"], { env: { ...process.env, COPYFILE_DISABLE: "1" } });
        inventory.assets.push({ target, accelerator, file, sha256: await sha256File(archive), bytes: (await stat(archive)).size });
      }
    }
    // Both installers read this fixed column format without extra dependencies or evaluating code.
    await writeFile(join(outputDir, "manifest.tsv"), serializeStandaloneManifest(inventory));
    for (const name of ["install.sh", "install.ps1"]) await copyFile(join(workspaceDir, "standalone", name), join(outputDir, name));
    const checksums = await Promise.all([...inventory.assets.map((asset) => asset.file), "manifest.tsv", "install.sh", "install.ps1"]
      .map(async (file) => `${await sha256File(join(outputDir, file))}  ${file}\n`));
    await writeFile(join(outputDir, "SHA256SUMS"), checksums.join(""));
    return inventory;
  } finally { await rm(work, { recursive: true, force: true }); }
}

async function main(args) {
  const options = { tarballRoot: join(workspaceDir, "dist/npm/release/tarballs"), outputDir: join(workspaceDir, "dist/standalone") };
  for (let index = 0; index < args.length; index += 2) {
    const key = { "--tarball-dir": "tarballRoot", "--output-dir": "outputDir" }[args[index]];
    if (!key || !args[index + 1]) throw new Error("usage: standalone-release.mjs [--tarball-dir PATH] [--output-dir PATH]");
    options[key] = resolve(args[index + 1]);
  }
  const release = JSON.parse(await readFile(join(options.tarballRoot, "release.json"), "utf8"));
  const result = await collectStandaloneRelease({ ...options, release });
  console.log(`Collected ${result.assets.length} standalone archives for ${result.version}`);
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) main(process.argv.slice(2)).catch((error) => {
  console.error(`standalone-release: ${error.message}`);
  process.exitCode = 1;
});
