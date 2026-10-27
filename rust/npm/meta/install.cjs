"use strict";

const { createHash, randomBytes } = require("crypto");
const {
  chmodSync,
  closeSync,
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readdirSync,
  readFileSync,
  readSync,
  renameSync,
  rmdirSync,
  rmSync,
  writeSync,
  writeFileSync,
} = require("fs");
const { dirname, isAbsolute, join, normalize, relative, resolve, sep } = require("path");
const plugins = require("./plugins.cjs");

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
  const hash = createHash("sha256");
  const fd = openSync(path, "r");
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  try {
    let count;
    while ((count = readSync(fd, buffer, 0, buffer.length, null)) > 0) hash.update(buffer.subarray(0, count));
  } finally {
    closeSync(fd);
  }
  return hash.digest("hex");
}

function checkedPayloadPath(root, payloadPath) {
  if (!/^(bin|lib|parts)\/[a-zA-Z0-9_./-]+$/.test(payloadPath) ||
      payloadPath.split("/").some((part) => !part || part === "." || part === "..")) {
    throw new Error(`invalid native payload path: ${payloadPath}`);
  }
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

function verifyPackageIdentity(root, name, expectedVersion) {
  const packageJson = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
  if (packageJson.name !== name || packageJson.version !== expectedVersion) {
    throw new Error(
      `native package mismatch: expected ${name}@${expectedVersion}, got ${packageJson.name}@${packageJson.version}`,
    );
  }
  return packageJson;
}

function verifyPayload(platformRoot, entry, expectedVersion) {
  verifyPackageIdentity(platformRoot, entry.package, expectedVersion);
  const checksumPath = join(platformRoot, "checksums.json");
  const checksums = JSON.parse(readFileSync(checksumPath, "utf8"));
  if (![1, 3].includes(checksums.schemaVersion) || checksums.target !== entry.target) {
    throw new Error(`invalid native checksum manifest for ${entry.package}`);
  }
  const expectedBinary = `bin/${entry.binary}`;
  if (!checksums.files || !checksums.files[expectedBinary]) {
    throw new Error(`native checksum manifest does not contain ${expectedBinary}`);
  }

  // Schema 1 is a self-contained package; schema 3 declares optional plugins separately.
  for (const [file, expected] of Object.entries(checksums.files)) {
    checkedPayloadPath(platformRoot, file);
    if (!Number.isSafeInteger(expected.bytes) || expected.bytes < 0 || !/^[a-f0-9]{64}$/.test(expected.sha256)) {
      throw new Error(`invalid native checksum: ${file}`);
    }
  }
  verifyFiles(platformRoot, checksums.files);
  if (checksums.schemaVersion === 3) readPlugins(platformRoot, entry, expectedVersion, checksums);
  return checksums;
}

function readPlugins(platformRoot, entry, version, checksums) {
  const metadata = verifyPackageIdentity(platformRoot, entry.package, version);
  if (Object.keys(metadata.dependencies || {}).length ||
      Object.keys(metadata.optionalDependencies || {}).length ||
      checksums.coreId !== plugins.coreIdentity(entry.target, version, checksums.files) ||
      digest(join(platformRoot, "plugins.json")) !== checksums.pluginsSha256) {
    throw new Error("invalid base/plugin manifest binding");
  }
  const manifest = plugins.validatePlugins(JSON.parse(readFileSync(join(platformRoot, "plugins.json"), "utf8")),
    entry, version, checksums.coreId);
  for (const file of Object.keys(manifest.files)) {
    checkedPayloadPath(platformRoot, file);
    if (!file.startsWith("lib/") || checksums.files[file]) throw new Error("plugin may not replace a base file");
  }
  for (const component of manifest.components) checkedPayloadPath(platformRoot, component.payload);
  return manifest;
}

function verifyFiles(root, files) {
  const declaredFiles = Object.keys(files).sort();
  const actualFiles = ["bin", "lib", "parts"]
    .filter((directory) => existsSync(join(root, directory)))
    .flatMap((directory) => listPayloadFiles(root, join(root, directory)))
    .sort();
  if (JSON.stringify(actualFiles) !== JSON.stringify(declaredFiles)) {
    const missing = declaredFiles.filter((file) => !actualFiles.includes(file));
    const unexpected = actualFiles.filter((file) => !declaredFiles.includes(file));
    throw new Error(`native payload files do not match the checksum manifest (missing: ${missing.join(", ")}; unexpected: ${unexpected.join(", ")})`);
  }

  for (const [payloadPath, expected] of Object.entries(files)) {
    const absolutePath = checkedPayloadPath(root, payloadPath);
    const metadata = lstatSync(absolutePath);
    if (!metadata.isFile()) {
      throw new Error(`native payload is not a file: ${payloadPath}`);
    }
    if (metadata.size !== expected.bytes || digest(absolutePath) !== expected.sha256) {
      throw new Error(`native payload checksum mismatch: ${payloadPath}`);
    }
  }
}

// Also used by release verification: a successful CPU CLI must not hide missing
// or corrupt GPU components, even when no GPU is present on the verification host.
function verifyInstallation({ metaRoot, platformRoot, entry, expectedVersion, requireCuda = false }) {
  const checksums = verifyPayload(platformRoot, entry, expectedVersion);
  if (checksums.schemaVersion === 3) {
    const manifest = readPlugins(platformRoot, entry, expectedVersion, checksums);
    const receipt = JSON.parse(readFileSync(join(metaRoot, "native-install.json"), "utf8"));
    if (receipt.target !== entry.target || receipt.version !== expectedVersion || receipt.coreId !== manifest.coreId ||
        !Array.isArray(receipt.components) ||
        (receipt.components.length && JSON.stringify(receipt.components) !== JSON.stringify(manifest.components.map((c) => c.package))) ||
        (requireCuda && receipt.components.length !== manifest.components.length)) {
      throw new Error("installed plugin identity/core ABI mismatch or required CUDA plugins missing");
    }
    verifyFiles(metaRoot, installedFiles(entry, { ...checksums.files,
      ...(receipt.components.length ? manifest.files : {}) }));
    return { ...receipt, plugins: receipt.components.length ? manifest.components : [] };
  }
  if (requireCuda) throw new Error("required CUDA plugins missing");
  verifyFiles(metaRoot, installedFiles(entry, checksums.files));
  return { target: entry.target, version: expectedVersion, components: [] };
}

function materializeBase({ metaRoot, platformRoot, entry, expectedVersion }) {
  const checksums = verifyPayload(platformRoot, entry, expectedVersion);

  const temporaryRoot = join(metaRoot, `.native-${process.pid}.tmp`);
  const metaBin = join(metaRoot, "bin");
  const sourceBin = join(platformRoot, "bin");
  const sourceLib = join(platformRoot, "lib");
  const stagedBin = join(temporaryRoot, "bin");
  const stagedLib = join(temporaryRoot, "lib");
  rmSync(temporaryRoot, { recursive: true, force: true });
  mkdirSync(temporaryRoot, { recursive: true });

  try {
    const nativeEntries = readdirSync(sourceBin);
    if (nativeEntries.length !== 1 || nativeEntries[0] !== entry.binary) {
      throw new Error("native package bin/ must contain only the zg executable");
    }
    mkdirSync(stagedBin, { recursive: true });
    copyFileSync(join(sourceBin, entry.binary), join(stagedBin, entry.binary));
    copyTreeSync(sourceLib, stagedLib);

    // Verify the complete CPU base before touching the previous installation.
    verifyFiles(temporaryRoot, checksums.files);

    mkdirSync(metaBin, { recursive: true });
    const metaLib = join(metaRoot, "lib");
    const finalBinary = join(metaBin, "zg.exe");
    const temporaryBinary = join(metaBin, `.zg-${process.pid}.tmp`);
    copyFileSync(join(stagedBin, entry.binary), temporaryBinary);
    if (process.platform !== "win32") {
      chmodSync(temporaryBinary, 0o755);
    }
    const previousLib = join(temporaryRoot, "previous-lib");
    const previousBinary = join(temporaryRoot, "previous-binary");
    let movedLib = false;
    let movedBinary = false;
    let installedLib = false;
    try {
      if (existsSync(metaLib)) { renameSync(metaLib, previousLib); movedLib = true; }
      if (existsSync(finalBinary)) { renameSync(finalBinary, previousBinary); movedBinary = true; }
      renameSync(stagedLib, metaLib);
      installedLib = true;
      renameSync(temporaryBinary, finalBinary);
    } catch (error) {
      if (installedLib) rmSync(metaLib, { recursive: true, force: true });
      if (movedLib) renameSync(previousLib, metaLib);
      if (movedBinary) renameSync(previousBinary, finalBinary);
      throw error;
    } finally {
      rmSync(temporaryBinary, { force: true });
    }
  } finally {
    rmSync(temporaryRoot, { recursive: true, force: true });
  }
}

function installedFiles(entry, files) {
  return Object.fromEntries(Object.entries(files).map(([file, value]) =>
    [file === `bin/${entry.binary}` ? "bin/zg.exe" : file, value]));
}

function writeReceipt(metaRoot, receipt) {
  const temporary = join(metaRoot, `.native-install-${process.pid}.json`);
  writeFileSync(temporary, `${JSON.stringify({ schemaVersion: 1, target: receipt.target,
    version: receipt.version, coreId: receipt.coreId, components: receipt.components }, null, 2)}\n`);
  renameSync(temporary, join(metaRoot, "native-install.json"));
}

function readInstallLock(path) {
  let identity;
  try { identity = readFileSync(path, "utf8"); } catch (error) {
    if (error.code === "ENOENT") return undefined;
    throw error;
  }
  let record;
  try { record = JSON.parse(identity); } catch { /* invalid/partially written lock: fail closed */ }
  // Retain recovery of the previous PID-only format.
  const pid = typeof record === "number" ? record : record?.pid;
  if (!Number.isSafeInteger(pid) || pid <= 0 ||
      (typeof record !== "number" && typeof record?.token !== "string")) {
    throw new Error("invalid installation lock; inspect .native-install.lock before retrying");
  }
  return { pid, identity };
}

function ownerAlive(pid) {
  try { process.kill(pid, 0); } catch (error) { if (error.code === "ESRCH") return false; }
  return true; // Permission/query errors must not authorize deleting a lock.
}

function openInstallLock(path) {
  try { return openSync(path, "wx"); } catch (error) {
    if (error.code !== "EEXIST") throw error;
    throw new Error("another native installation is in progress; retry after it finishes");
  }
}

function acquireInstallLock(path) {
  const previous = readInstallLock(path);
  if (!previous) return openInstallLock(path);
  if (ownerAlive(previous.pid)) throw new Error("another native installation is in progress; retry after it finishes");
  const recovery = `${path}.recovery`;
  // Only one stale-owner recovery may inspect/remove/replace the lock at once.
  // Never reclaim this short-lived guard automatically: doing so would recreate
  // the same race one level up. A crash here requires manual inspection.
  try { mkdirSync(recovery); } catch (error) {
    if (error.code !== "EEXIST") throw error;
    throw new Error("native lock recovery is in progress; if no installer is running, inspect .native-install.lock.recovery before retrying");
  }
  try {
    const current = readInstallLock(path);
    if (current && (current.identity !== previous.identity || ownerAlive(current.pid))) {
      throw new Error("installation lock owner changed; retry after the current installation finishes");
    }
    if (current) rmSync(path);
    // A fresh installer may win this gap; exclusive create must then fail,
    // without removing its replacement lock.
    return openInstallLock(path);
  } finally { rmdirSync(recovery); }
}

function installLock(metaRoot, action) {
  mkdirSync(metaRoot, { recursive: true });
  const path = join(metaRoot, ".native-install.lock");
  const identity = JSON.stringify({ pid: process.pid, token: randomBytes(16).toString("hex") });
  const fd = acquireInstallLock(path);
  try {
    writeFileSync(fd, identity);
    return action();
  } finally {
    closeSync(fd);
    // A live owner cannot be reclaimed by this protocol. Also refuse to delete
    // a lock replaced externally (or by a different invocation in this process).
    if (readInstallLock(path)?.identity === identity) rmSync(path);
  }
}

function assemblePlugins(manifest, config, temporary, fetchArchive) {
  for (const [index, component] of manifest.components.entries()) {
    const archive = fetchArchive(component, manifest, config);
    const part = join(temporary, `part-${index}`);
    plugins.extractPayload(archive, component, manifest, part);
    const destination = checkedPayloadPath(temporary, component.file);
    mkdirSync(dirname(destination), { recursive: true });
    const input = openSync(part, "r");
    const output = openSync(destination, "a", 0o644);
    const buffer = Buffer.allocUnsafe(1024 * 1024);
    try {
      let count;
      while ((count = readSync(input, buffer, 0, buffer.length, null))) {
        let offset = 0;
        while (offset < count) offset += writeSync(output, buffer, offset, count - offset);
      }
    } finally { closeSync(input); closeSync(output); rmSync(part); }
  }
  verifyFiles(temporary, manifest.files);
}

function materialize(options) {
  const { metaRoot, platformRoot, entry, expectedVersion } = options;
  const config = options.config || plugins.settings();
  // Base corruption and invalid metadata are never optional failures.
  const checksums = verifyPayload(platformRoot, entry, expectedVersion);
  return installLock(metaRoot, () => {
    if (checksums.schemaVersion !== 3) {
      materializeBase(options);
      if (config.mode === "cuda") throw new Error("this native package has no CUDA plugin support");
      return verifyInstallation(options);
    }
    const manifest = readPlugins(platformRoot, entry, expectedVersion, checksums);
    let installed;
    try { installed = verifyInstallation(options); } catch { /* bootstrap or repair from the verified base */ }
    if (!installed) {
      materializeBase(options);
      installed = { schemaVersion: 1, target: entry.target, version: expectedVersion,
        coreId: manifest.coreId, components: [] };
      writeReceipt(metaRoot, installed);
    }
    if (config.mode === "cpu") return installed;
    if (installed.components.length) return installed;
    const wantsCuda = config.mode === "cuda" || (options.detectNvidia || plugins.detectNvidia)();
    if (!wantsCuda) {
      console.log("zvec-grep: CPU runtime installed; CUDA can be added with ZG_INSTALL_ACCELERATOR=cuda npm rebuild");
      return installed;
    }
    const temporary = mkdtempSync(join(metaRoot, ".plugins-"));
    const activated = [];
    try {
      assemblePlugins(manifest, config, temporary, options.fetchArchive || plugins.componentArchive);
      // Every complete library is verified before activation. Core files are
      // untouched. Stop zg/daemon before installing; locked Windows DLLs fail.
      for (const file of Object.keys(manifest.files)) {
        const destination = checkedPayloadPath(metaRoot, file);
        mkdirSync(dirname(destination), { recursive: true });
        if (existsSync(destination)) throw new Error(`plugin destination is occupied: ${file}; stop zg and retry`);
        renameSync(checkedPayloadPath(temporary, file), destination);
        activated.push(destination);
      }
      writeReceipt(metaRoot, { ...installed, components: manifest.components.map((c) => c.package) });
      console.log("zvec-grep: verified CUDA plugins installed; restart any running zg daemon. Host CUDA libraries/drivers are still required.");
      return verifyInstallation({ ...options, requireCuda: true });
    } catch (error) {
      for (const path of activated.reverse()) rmSync(path, { force: true });
      writeReceipt(metaRoot, installed);
      if (config.mode === "cuda") throw error;
      console.warn(`zvec-grep: optional CUDA installation failed: ${error.message}; CPU remains usable. Retry with ZG_INSTALL_ACCELERATOR=cuda npm rebuild.`);
      return installed;
    } finally { rmSync(temporary, { recursive: true, force: true }); }
  });
}

function resolvePackageRoot(metaRoot, packageName) {
  try {
    return dirname(require.resolve(`${packageName}/package.json`, { paths: [resolve(metaRoot)] }));
  } catch (error) {
    const detail = error && error.code === "MODULE_NOT_FOUND" ? "" : ` (${error.message})`;
    throw new Error(
      `missing ${packageName}; reinstall without --omit=optional or install the exact-version platform base tarball${detail}`,
    );
  }
}

function resolvePlatformRoot(metaRoot, entry) {
  return resolvePackageRoot(metaRoot, entry.package);
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
  verifyInstallation,
  readPlugins,
};

if (require.main === module) {
  try {
    main();
  } catch (error) {
    console.error(`zvec-grep install: ${error.message}`);
    process.exitCode = 1;
  }
}
