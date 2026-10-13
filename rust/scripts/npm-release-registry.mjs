import { createHash } from "node:crypto";
import { appendFile, readFile, readdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { run, sha256File } from "./npm-package-core.mjs";
import { verifyTarballCompliance } from "./npm-release-compliance.mjs";

function filename(name, version) {
  return `${name.replace(/^@/, "").replaceAll("/", "-")}-${version}.tgz`;
}

function expectedPackages(manifest, version) {
  return [
    ...manifest.platforms.map((entry) => ({
      name: entry.package,
      target: entry.target,
    })),
    { name: manifest.metaPackage, target: "meta" },
  ].map((entry) => ({
    ...entry,
    version,
    file: filename(entry.name, version),
  }));
}

export async function collectRelease({ manifest, version, tarballRoot }) {
  const packages = expectedPackages(manifest, version);
  const actual = (await readdir(tarballRoot))
    .filter((name) => name.endsWith(".tgz"))
    .sort();
  if (
    JSON.stringify(actual) !==
    JSON.stringify(packages.map((entry) => entry.file).sort())
  ) {
    throw new Error(
      "release tarballs do not cover exactly the published platform manifest and meta package",
    );
  }
  for (const entry of packages) {
    const path = join(tarballRoot, entry.file);
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
        !metadata.cpu?.includes(platform.cpu)
      ) {
        throw new Error(`release package platform mismatch: ${entry.file}`);
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
    entry.integrity = `sha512-${createHash("sha512")
      .update(await readFile(path))
      .digest("base64")}`;
  }
  const release = {
    schemaVersion: 1,
    version,
    commit:
      process.env.GITHUB_SHA ??
      run("git", ["rev-parse", "HEAD"], { capture: true }).trim(),
    repository: process.env.GITHUB_REPOSITORY ?? "zvec-ai/zvec-grep",
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
    `# Rust zvec-grep ${version}`,
    "",
    `Source commit: ${release.commit}`,
    "",
    "Unzip the Actions artifact, then install the meta tarball together with the native tarball for your machine:",
    "",
    "```sh",
    ...manifest.platforms.map(
      (platform) =>
        `# ${platform.target}\nnpm install --global --allow-scripts=./${filename(manifest.metaPackage, version)} ./${filename(platform.package, version)} ./${filename(manifest.metaPackage, version)}`,
    ),
    "zg --version",
    "```",
    "",
    "Use only the command for your platform. On Windows, use zg.cmd if PowerShell blocks the generated zg.ps1 shim.",
    "The native packages contain the executable, shared runtime libraries, and Jieba dictionaries.",
    "Linux candidates are built and verified on Ubuntu 24.04; older glibc and C++ runtimes are not covered.",
    "SHA256SUMS and release.json identify the exact files and source commit.",
    "These artifacts use the regular npm package layout; no registry publication is needed to install them.",
    "macOS packages use the toolchain's local code signatures; Windows packages do not require a signing certificate.",
    "",
  ].join("\n");
  await writeFile(join(tarballRoot, "README.md"), instructions);
  if (process.env.GITHUB_STEP_SUMMARY) {
    await appendFile(
      process.env.GITHUB_STEP_SUMMARY,
      `## Rust release ${version}\n\nSource: \`${release.commit}\`\n\nDownload **rust-release-${version}** for all ${manifest.platforms.length} native packages, the meta package, checksums, and installation instructions.\n`,
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
      entry.target !== identity.target
    ) {
      throw new Error(
        "release package order or identity does not match the platform manifest",
      );
    }
    if ((await sha256File(join(tarballRoot, entry.file))) !== entry.sha256) {
      throw new Error(`release tarball checksum mismatch: ${entry.file}`);
    }
    if (entry.target !== "meta")
      verifyTarballCompliance(join(tarballRoot, entry.file));
    published.push(await checkPublishedPackage(entry, registryFetch));
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
