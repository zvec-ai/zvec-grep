"use strict";

const { createHash } = require("crypto");
const {
  chmodSync,
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  renameSync,
  rmSync,
} = require("fs");
const { dirname, isAbsolute, join, normalize, relative, resolve, sep } = require("path");

function copyTreeSync(source, destination) {
  const metadata = lstatSync(source);
  if (metadata.isDirectory()) {
    mkdirSync(destination, { recursive: true });
    for (const name of readdirSync(source)) {
      copyTreeSync(join(source, name), join(destination, name));
    }
    return;
  }
  if (!metadata.isFile()) {
    throw new Error(`native payload is not a regular file: ${source}`);
  }
  mkdirSync(dirname(destination), { recursive: true });
  copyFileSync(source, destination);
}

function digest(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function checkedPayloadPath(root, payloadPath) {
  if (isAbsolute(payloadPath)) {
    throw new Error(`native checksum contains an absolute path: ${payloadPath}`);
  }
  const normalized = normalize(payloadPath);
  if (normalized === ".." || normalized.startsWith(`..${sep}`)) {
    throw new Error(`native checksum escapes the package: ${payloadPath}`);
  }
  const canonicalRoot = resolve(root);
  const resolved = resolve(canonicalRoot, normalized);
  const relativePath = relative(canonicalRoot, resolved);
  if (relativePath === ".." || relativePath.startsWith(`..${sep}`) || isAbsolute(relativePath)) {
    throw new Error(`native checksum escapes the package: ${payloadPath}`);
  }
  return resolved;
}

function listPayloadFiles(root, directory = root) {
  const files = [];
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      files.push(...listPayloadFiles(root, path));
    } else {
      files.push(relative(root, path).split(sep).join("/"));
    }
  }
  return files;
}

function verifyPayload(platformRoot, entry, expectedVersion) {
  const packageJson = JSON.parse(readFileSync(join(platformRoot, "package.json"), "utf8"));
  if (packageJson.name !== entry.package || packageJson.version !== expectedVersion) {
    throw new Error(
      `native package mismatch: expected ${entry.package}@${expectedVersion}, got ${packageJson.name}@${packageJson.version}`,
    );
  }

  const checksumPath = join(platformRoot, "checksums.json");
  const checksums = JSON.parse(readFileSync(checksumPath, "utf8"));
  if (checksums.schemaVersion !== 1 || checksums.target !== entry.target) {
    throw new Error(`invalid native checksum manifest for ${entry.package}`);
  }
  const expectedBinary = `bin/${entry.binary}`;
  if (!checksums.files || !checksums.files[expectedBinary]) {
    throw new Error(`native checksum manifest does not contain ${expectedBinary}`);
  }

  const declaredFiles = Object.keys(checksums.files).sort();
  const actualFiles = ["bin", "lib"]
    .filter((directory) => existsSync(join(platformRoot, directory)))
    .flatMap((directory) => listPayloadFiles(platformRoot, join(platformRoot, directory)))
    .sort();
  if (JSON.stringify(actualFiles) !== JSON.stringify(declaredFiles)) {
    throw new Error("native payload files do not match the checksum manifest");
  }

  for (const [payloadPath, expected] of Object.entries(checksums.files)) {
    const absolutePath = checkedPayloadPath(platformRoot, payloadPath);
    const metadata = lstatSync(absolutePath);
    if (!metadata.isFile()) {
      throw new Error(`native payload is not a file: ${payloadPath}`);
    }
    if (metadata.size !== expected.bytes || digest(absolutePath) !== expected.sha256) {
      throw new Error(`native payload checksum mismatch: ${payloadPath}`);
    }
  }
  return checksums;
}

function materialize({ metaRoot, platformRoot, entry, expectedVersion }) {
  verifyPayload(platformRoot, entry, expectedVersion);

  const temporaryRoot = join(metaRoot, `.native-${process.pid}.tmp`);
  const metaBin = join(metaRoot, "bin");
  const sourceBin = join(platformRoot, "bin");
  const sourceLib = join(platformRoot, "lib");
  const stagedBin = join(temporaryRoot, "bin");
  const stagedLib = join(temporaryRoot, "lib");
  rmSync(temporaryRoot, { recursive: true, force: true });
  mkdirSync(temporaryRoot, { recursive: true });

  try {
    copyTreeSync(sourceBin, stagedBin);
    if (existsSync(sourceLib)) {
      copyTreeSync(sourceLib, stagedLib);
    }

    mkdirSync(metaBin, { recursive: true });
    for (const name of readdirSync(stagedBin)) {
      if (name === entry.binary) {
        continue;
      }
      copyTreeSync(join(stagedBin, name), join(metaBin, name));
    }

    if (existsSync(stagedLib)) {
      const metaLib = join(metaRoot, "lib");
      rmSync(metaLib, { recursive: true, force: true });
      renameSync(stagedLib, metaLib);
    }

    const finalBinary = join(metaBin, "zg.exe");
    const temporaryBinary = join(metaBin, `.zg-${process.pid}.tmp`);
    copyFileSync(join(stagedBin, entry.binary), temporaryBinary);
    if (process.platform !== "win32") {
      chmodSync(temporaryBinary, 0o755);
    } else {
      rmSync(finalBinary, { force: true });
    }
    renameSync(temporaryBinary, finalBinary);
  } finally {
    rmSync(temporaryRoot, { recursive: true, force: true });
  }
}

function resolvePlatformRoot(metaRoot, entry) {
  try {
    return dirname(require.resolve(`${entry.package}/package.json`, { paths: [metaRoot] }));
  } catch (error) {
    const detail = error && error.code === "MODULE_NOT_FOUND" ? "" : ` (${error.message})`;
    throw new Error(
      `missing ${entry.package}; reinstall without --omit=optional or install that exact platform package${detail}`,
    );
  }
}

function selectPlatform(platforms, platform, arch, reportHeader = {}) {
  const libc = platform === "linux"
    ? (reportHeader.glibcVersionRuntime ? "glibc" : "musl")
    : undefined;
  const matches = platforms.filter((entry) =>
    entry.os === platform
    && entry.cpu === arch
    && (entry.libc === undefined || entry.libc === libc));
  if (matches.length !== 1) {
    const suffix = libc ? `-${libc}` : "";
    throw new Error(`zvec-grep does not publish a native package for ${platform}-${arch}${suffix}`);
  }
  return matches[0];
}

function main() {
  const metaRoot = __dirname;
  const packageJson = JSON.parse(readFileSync(join(metaRoot, "package.json"), "utf8"));
  const platforms = JSON.parse(readFileSync(join(metaRoot, "platforms.json"), "utf8"));
  const report = process.report && process.report.getReport();
  const entry = selectPlatform(platforms, process.platform, process.arch, report?.header ?? {});
  const platformRoot = resolvePlatformRoot(metaRoot, entry);
  materialize({
    metaRoot,
    platformRoot,
    entry,
    expectedVersion: packageJson.version,
  });
}

module.exports = {
  materialize,
  resolvePlatformRoot,
  selectPlatform,
  verifyPayload,
};

if (require.main === module) {
  try {
    main();
  } catch (error) {
    console.error(`zvec-grep install: ${error.message}`);
    process.exitCode = 1;
  }
}
