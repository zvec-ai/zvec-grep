import { appendFile, copyFile, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { hashFile, nativePackageEntries, run, sha256File, verifyTarballSize } from "./npm-package-core.mjs";
import { verifyTarballCompliance } from "./npm-release-compliance.mjs";
import { coreIdentity, validatePlugins } from "../npm/meta/plugins.cjs";

function verifyPluginReferences(platform, version, packages, tarballRoot) {
  const base = packages.find((pkg) => pkg.name === platform.package);
  const path = join(tarballRoot, base.file);
  const read = (file, archive = path) => run("tar", ["-xOf", archive, `package/${file}`], { capture: true });
  const metadata = JSON.parse(read("package.json"));
  const checksums = JSON.parse(read("checksums.json"));
  if (checksums.schemaVersion !== 3 || Object.keys(metadata.dependencies || {}).length ||
      Object.keys(metadata.optionalDependencies || {}).length ||
      checksums.coreId !== coreIdentity(platform.target, version, checksums.files)) {
    throw new Error(`release platform does not lock every runtime component: ${base.file}`);
  }
  const raw = read("plugins.json");
  if (createHash("sha256").update(raw).digest("hex") !== checksums.pluginsSha256) {
    throw new Error("release plugin manifest hash mismatch");
  }
  const plugins = validatePlugins(JSON.parse(raw), platform, version, checksums.coreId);
  for (const component of plugins.components) {
    const archive = packages.find((pkg) => pkg.name === component.package);
    if (!archive || archive.version !== component.version || archive.file !== component.tarball ||
        archive.integrity !== component.integrity || archive.bytes !== component.archiveBytes ||
        checksums.files[component.file]) throw new Error(`release plugin reference mismatch: ${component.package}`);
    const payload = JSON.parse(read("checksums.json", join(tarballRoot, archive.file))).files[component.payload];
    if (payload?.sha256 !== component.checksum.sha256 || payload?.bytes !== component.checksum.bytes) {
      throw new Error(`release plugin payload mismatch: ${component.package}`);
    }
  }
}

function filename(name, version) {
  return `${name.replace(/^@/, "").replaceAll("/", "-")}-${version}.tgz`;
}

function expectedPackages(manifest, version) {
  return [
    ...manifest.platforms.flatMap(nativePackageEntries),
    { name: manifest.metaPackage, target: "meta" },
  ].map((entry) => ({
    ...entry,
    version,
    file: filename(entry.name, version),
  }));
}

// Validation runners need only their native artifact plus these small, exact
// collected files, not another copy of every platform's CUDA payload.
export async function stageVerificationMetadata({ release, tarballRoot, destination }) {
  const meta = release.packages.filter((entry) => entry.target === "meta");
  if (meta.length !== 1 || meta[0].file !== filename(meta[0].name, release.version) ||
      await sha256File(join(tarballRoot, meta[0].file)) !== meta[0].sha256) {
    throw new Error("invalid collected meta package");
  }
  await rm(destination, { recursive: true, force: true });
  await mkdir(destination, { recursive: true });
  for (const file of [meta[0].file, "release.json", "SHA256SUMS", "README.md"]) {
    await copyFile(join(tarballRoot, file), join(destination, file));
  }
}

export async function collectRelease({ manifest, version, tarballRoot, targets }) {
  if (
    targets !== undefined &&
    (!Array.isArray(targets) ||
      targets.length === 0 ||
      new Set(targets).size !== targets.length ||
      targets.some(
        (target) => !manifest.platforms.some((entry) => entry.target === target),
      ))
  ) {
    throw new Error(
      "collect targets must be a nonempty list of unique known platforms",
    );
  }
  const platforms = manifest.platforms.filter(
    (entry) => targets === undefined || targets.includes(entry.target),
  );
  const selectedTargets = platforms.map((entry) => entry.target);
  const partial = platforms.length !== manifest.platforms.length;
  const packages = expectedPackages(manifest, version).filter(
    (entry) => entry.target === "meta" || selectedTargets.includes(entry.target),
  );
  const actual = (await readdir(tarballRoot))
    .filter((name) => name.endsWith(".tgz"))
    .sort();
  if (
    JSON.stringify(actual) !==
    JSON.stringify(packages.map((entry) => entry.file).sort())
  ) {
    throw new Error(
      "release tarballs do not cover exactly the selected platform manifest and meta package",
    );
  }
  for (const entry of packages) {
    const path = join(tarballRoot, entry.file);
    entry.bytes = await verifyTarballSize(path);
    const metadata = JSON.parse(
      run("tar", ["-xOf", path, "package/package.json"], { capture: true }),
    );
    if (metadata.name !== entry.name || metadata.version !== version) {
      throw new Error(`release package identity mismatch: ${entry.file}`);
    }
    if (entry.target !== "meta") {
      const platform = manifest.platforms.find(
        (candidate) => candidate.target === entry.target,
      );
      const checksums = JSON.parse(
        run("tar", ["-xOf", path, "package/checksums.json"], { capture: true }),
      );
      if (
        checksums.target !== entry.target ||
        !metadata.os?.includes(platform.os) ||
        !metadata.cpu?.includes(platform.cpu) ||
        (platform.libc && !metadata.libc?.includes(platform.libc))
      ) {
        throw new Error(`release package platform mismatch: ${entry.file}`);
      }
      if (entry.component) {
        const component = platform.components.find((candidate) => candidate.package === entry.component);
        if (checksums.schemaVersion !== 1 || checksums.component !== entry.component ||
            JSON.stringify(Object.keys(checksums.files ?? {})) !== JSON.stringify([component.payload])) {
          throw new Error(`release component manifest mismatch: ${entry.file}`);
        }
      }
    } else {
      const expected = Object.fromEntries(
        manifest.platforms.map((platform) => [platform.package, version]),
      );
      if (
        JSON.stringify(metadata.optionalDependencies) !==
        JSON.stringify(expected)
      ) {
        throw new Error(
          "release meta package does not lock every platform to the release version",
        );
      }
    }
    entry.sha256 = await sha256File(path);
    entry.integrity = `sha512-${await hashFile(path, "sha512", "base64")}`;
  }
  for (const platform of platforms.filter((entry) => entry.components?.length)) {
    verifyPluginReferences(platform, version, packages, tarballRoot);
  }
  const release = {
    schemaVersion: 1,
    version,
    commit:
      process.env.GITHUB_SHA ??
      run("git", ["rev-parse", "HEAD"], { capture: true }).trim(),
    repository: process.env.GITHUB_REPOSITORY ?? "zvec-ai/zvec-grep",
    partial,
    targets: selectedTargets,
    packages,
  };
  await writeFile(
    join(tarballRoot, "release.json"),
    `${JSON.stringify(release, null, 2)}\n`,
  );
  await writeFile(
    join(tarballRoot, "SHA256SUMS"),
    packages.map((entry) => `${entry.sha256}  ${entry.file}\n`).join(""),
  );
  const instructions = [
    `# Rust zvec-grep ${version}${partial ? " — partial validation candidate" : ""}`,
    "",
    `Source commit: ${release.commit}`,
    "",
    "Collection checks completeness and package identity, not runtime success. Check the workflow's CPU/GPU validation results before using these candidates; collection alone does not approve publication.",
    "",
    ...(partial
      ? [
          "Validation only: this is a partial candidate, not a complete release. It cannot be published or promoted.",
          `Included platforms: ${selectedTargets.join(", ")}.`,
          "The unchanged meta package still locks every supported platform to this version; only the native tarballs listed below are included.",
          "",
        ]
      : []),
    "Unzip the Actions artifact, then install the meta and base tarballs for your machine. For optional CUDA installation, set ZG_PLUGIN_DIR to the absolute artifact directory (PowerShell: $env:ZG_PLUGIN_DIR = (Get-Location).Path). This source is authoritative and never falls back to the registry:",
    "",
    "```sh",
    ...platforms.map(
      (platform) =>
        `# ${platform.target}\nnpm install --global --allow-scripts=./${filename(manifest.metaPackage, version)} ./${filename(platform.package, version)} ./${filename(manifest.metaPackage, version)}`,
    ),
    "zg --version",
    "```",
    "",
    "Use only the command for your platform. On Windows, use zg.cmd if PowerShell blocks the generated zg.ps1 shim.",
    "The native packages contain the executable and shared runtime libraries. The zvec library embeds its Jieba dictionary; no external dictionary files are required.",
    "ZG_INSTALL_ACCELERATOR=auto (default) installs CPU first and requests CUDA plugins only when NVIDIA detection succeeds; cpu skips detection/downloads; cuda forces plugin installation and fails if unsuccessful. Later use npm rebuild -g @zvec/zvec-grep with the same settings; no compilation is performed. Stop zg/daemon first and restart it afterwards.",
    "CPU installs do not require CUDA component tarballs. CUDA plugins are exact-version, core-bound and integrity-checked; Linux ORT fragments are concatenated and checked before activation. ZG_INSTALL_OFFLINE=1 disables network downloads; ZG_PLUGIN_DIR selects local archives. --ignore-scripts skips even CPU bootstrap: explicitly permit the package's scripts and run npm rebuild to initialize it.",
    "Every compressed npm tarball must be at most 350 MiB; this is checked at pack, collect, and publication preflight.",
    "Linux candidates target Ubuntu 24.04; older glibc and C++ runtimes are not covered.",
    "SHA256SUMS and release.json identify the exact files and source commit.",
    "These artifacts use the regular npm package layout; no registry publication is needed to install them.",
    "macOS packages use the toolchain's local code signatures; Windows packages do not require a signing certificate.",
    "",
  ].join("\n");
  await writeFile(join(tarballRoot, "README.md"), instructions);
  if (process.env.GITHUB_STEP_SUMMARY) {
    await appendFile(
      process.env.GITHUB_STEP_SUMMARY,
      `## Rust ${partial ? "validation candidate" : "release"} ${version}\n\nSource: \`${release.commit}\`\n\nDownload **rust-${partial ? "candidate" : "release"}-${version}** for ${platforms.length} native targets (${selectedTargets.join(", ")}), all required runtime components, the meta package, checksums, and installation instructions.${partial ? " This partial candidate is validation-only and cannot be published or promoted." : ""}\n`,
    );
  }
  console.log(`collected ${packages.length} verified packages for ${version}`);
  return release;
}

export async function checkPublishedPackage(entry, registryFetch = fetch) {
  const url = `https://registry.npmjs.org/${encodeURIComponent(entry.name)}/${encodeURIComponent(entry.version)}`;
  const response = await registryFetch(url, {
    signal: AbortSignal.timeout(30_000),
  });
  if (response.status === 404) return false;
  if (!response.ok)
    throw new Error(
      `npm registry returned ${response.status} for ${entry.name}@${entry.version}`,
    );
  const metadata = await response.json();
  if (metadata.dist?.integrity !== entry.integrity) {
    throw new Error(
      `npm already contains different bytes for ${entry.name}@${entry.version}; choose a new version`,
    );
  }
  return true;
}

async function preflightRelease({ manifest, tarballRoot, registryFetch }) {
  const release = JSON.parse(
    await readFile(join(tarballRoot, "release.json"), "utf8"),
  );
  if (release.partial) {
    throw new Error(
      "partial validation candidates cannot be published or promoted",
    );
  }
  if (
    release.schemaVersion !== 1 ||
    !/^\d+\.\d+\.\d+(?:-[\w.-]+)?(?:\+[\w.-]+)?$/.test(release.version)
  ) {
    throw new Error("invalid release manifest");
  }
  const expected = expectedPackages(manifest, release.version);
  if (release.packages.length !== expected.length)
    throw new Error("incomplete release manifest");
  const published = [];
  // Complete preflight before publishing any file. A retry can reuse versions
  // already published by this run, but never replace different package bytes.
  for (const [index, entry] of release.packages.entries()) {
    const identity = expected[index];
    if (
      entry.name !== identity.name ||
      entry.file !== identity.file ||
      entry.version !== identity.version ||
      entry.target !== identity.target || entry.component !== identity.component
    ) {
      throw new Error(
        "release package order or identity does not match the platform manifest",
      );
    }
    await verifyTarballSize(join(tarballRoot, entry.file));
    if ((await sha256File(join(tarballRoot, entry.file))) !== entry.sha256) {
      throw new Error(`release tarball checksum mismatch: ${entry.file}`);
    }
    if (`sha512-${await hashFile(join(tarballRoot, entry.file), "sha512", "base64")}` !== entry.integrity) {
      throw new Error(`release tarball integrity mismatch: ${entry.file}`);
    }
    if (entry.target !== "meta")
      verifyTarballCompliance(join(tarballRoot, entry.file));
    published.push(await checkPublishedPackage(entry, registryFetch));
  }
  for (const platform of manifest.platforms.filter((entry) => entry.components?.length)) {
    verifyPluginReferences(platform, release.version, release.packages, tarballRoot);
  }
  return { release, published };
}

export async function publishRelease({
  manifest,
  tarballRoot,
  registryFetch = fetch,
  runNpm = run,
}) {
  const { release, published } = await preflightRelease({
    manifest,
    tarballRoot,
    registryFetch,
  });
  for (const [index, entry] of release.packages.entries()) {
    if (published[index]) {
      runNpm("npm", [
        "dist-tag",
        "add",
        `${entry.name}@${entry.version}`,
        "rust-canary",
      ]);
    } else {
      runNpm("npm", [
        "publish",
        join(tarballRoot, entry.file),
        "--access",
        "public",
        "--provenance",
        "--tag",
        "rust-canary",
      ]);
    }
  }
}

export async function promoteRelease({
  manifest,
  tarballRoot,
  registryFetch = fetch,
  runNpm = run,
}) {
  const { release, published } = await preflightRelease({
    manifest,
    tarballRoot,
    registryFetch,
  });
  if (published.some((exists) => !exists)) {
    throw new Error("cannot promote an incomplete registry release");
  }
  // The workflow calls this only after every registry smoke job has passed.
  // Native tags move first; the meta package becomes the default last.
  for (const entry of release.packages) {
    runNpm("npm", [
      "dist-tag",
      "add",
      `${entry.name}@${entry.version}`,
      "latest",
    ]);
  }
}
