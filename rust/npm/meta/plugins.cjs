"use strict";

// Installation-only transport. Normal zg execution never invokes Node or npm.
const { createHash } = require("crypto");
const { spawnSync } = require("child_process");
const { closeSync, existsSync, lstatSync, mkdirSync, mkdtempSync,
  openSync, readSync, renameSync, rmSync } = require("fs");
const { homedir } = require("os");
const { join, resolve } = require("path");

function hashFile(path, algorithm = "sha256", encoding = "hex") {
  const hash = createHash(algorithm);
  const fd = openSync(path, "r");
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  try {
    let count;
    while ((count = readSync(fd, buffer, 0, buffer.length, null))) hash.update(buffer.subarray(0, count));
  } finally { closeSync(fd); }
  return hash.digest(encoding);
}

function coreIdentity(target, version, files) {
  return createHash("sha256").update(JSON.stringify({ target, version,
    files: Object.fromEntries(Object.entries(files).sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)) })).digest("hex");
}

function packageFilename(name, version) {
  return `${name.replace(/^@/, "").replace(/\//g, "-")}-${version}.tgz`;
}

function validHash(value) {
  return value && Number.isSafeInteger(value.bytes) && value.bytes >= 0 && /^[a-f0-9]{64}$/.test(value.sha256);
}

function validatePlugins(manifest, entry, version, coreId) {
  if (manifest.schemaVersion !== 1 || manifest.target !== entry.target || manifest.version !== version ||
      manifest.coreId !== coreId || !/^[a-f0-9]{64}$/.test(coreId) ||
      !Array.isArray(manifest.components) || !entry.components?.length ||
      manifest.components.length !== entry.components.length || !manifest.files) {
    throw new Error("invalid plugin manifest identity/core ABI");
  }
  for (const [index, expected] of entry.components.entries()) {
    const component = manifest.components[index];
    if (component.package !== expected.package || component.version !== version ||
        component.file !== expected.file || component.payload !== expected.payload ||
        component.tarball !== packageFilename(expected.package, version) ||
        !/^sha512-[A-Za-z0-9+/]{86}==$/.test(component.integrity) ||
        !Number.isSafeInteger(component.archiveBytes) || component.archiveBytes <= 0 ||
        component.archiveBytes > 350 * 1024 * 1024 || !validHash(component.checksum) ||
        !validHash(manifest.files[expected.file])) {
      throw new Error(`invalid pinned plugin: ${expected.package}`);
    }
  }
  if (JSON.stringify(Object.keys(manifest.files).sort()) !==
      JSON.stringify([...new Set(entry.components.map((component) => component.file))].sort())) {
    throw new Error("plugin manifest contains unexpected runtime files");
  }
  return manifest;
}

function settings(env = process.env) {
  const mode = env.ZG_INSTALL_ACCELERATOR || "auto";
  if (!["auto", "cpu", "cuda"].includes(mode)) throw new Error("ZG_INSTALL_ACCELERATOR must be auto, cpu or cuda");
  return { mode,
    directory: env.ZG_PLUGIN_DIR ? resolve(env.ZG_PLUGIN_DIR) : undefined,
    cache: resolve(env.ZG_PLUGIN_CACHE || join(env.npm_config_cache || join(homedir(), ".npm"), "zg-plugins")),
    offline: [env.ZG_INSTALL_OFFLINE, env.npm_config_offline].some((value) => /^(true|1)$/i.test(value || "")),
  };
}

function detectNvidia() {
  const result = spawnSync("nvidia-smi", ["--query-gpu=name,driver_version", "--format=csv,noheader"], {
    encoding: "utf8", timeout: 5000, maxBuffer: 64 * 1024, windowsHide: true,
  });
  // A download hint, never proof that CUDA/cuDNN or a particular plugin works.
  return result.status === 0 && Boolean(result.stdout?.trim());
}

function verifyArchive(path, component) {
  const metadata = lstatSync(path);
  if (!metadata.isFile() || metadata.size !== component.archiveBytes ||
      `sha512-${hashFile(path, "sha512", "base64")}` !== component.integrity) {
    throw new Error(`plugin archive integrity mismatch: ${component.package}`);
  }
}

function download(component, directory, env = process.env) {
  const args = ["pack", `${component.package}@${component.version}`, "--ignore-scripts", "--json",
    "--pack-destination", directory, "--fetch-retries=1", "--fetch-timeout=60000"];
  // Use npm itself for registry, proxy, authentication, CA and cache handling.
  // Keeping the invocation's cwd also preserves its project .npmrc. Never run a
  // nested npm install or mutate the caller's node_modules/package-lock.
  const npmCli = env.npm_execpath;
  if (!npmCli || !existsSync(npmCli)) throw new Error("online plugin installation must run through npm install/rebuild");
  const version = spawnSync(process.execPath, [npmCli, "--version"], {
    encoding: "utf8", timeout: 10000, maxBuffer: 64 * 1024, windowsHide: true,
  });
  if (version.status !== 0 || !(Number(version.stdout.trim().split(".")[0]) >= 7)) {
    throw new Error("online plugin downloads require npm 7+ (--pack-destination); use ZG_PLUGIN_DIR for offline installation");
  }
  const result = spawnSync(process.execPath, [npmCli, ...args], {
    cwd: env.INIT_CWD || process.cwd(), env: { ...env, npm_config_ignore_scripts: "true" },
    encoding: "utf8", maxBuffer: 1024 * 1024, timeout: 180000, windowsHide: true,
  });
  if (result.error || result.status !== 0) {
    // Do not echo registry credentials or arbitrary npm configuration.
    throw new Error(`npm pack failed for ${component.package} (status ${result.status ?? result.error?.code}); check npm registry/network settings`);
  }
  return join(directory, component.tarball);
}

function componentArchive(component, manifest, config, fetch = download) {
  // An explicit local artifact directory is authoritative: no registry/cache
  // fallback may hide missing files in CI or an incomplete offline release.
  if (config.directory) {
    const path = join(config.directory, component.tarball);
    verifyArchive(path, component);
    return path;
  }
  // Hash the full identity into one path segment rather than nesting two long
  // hashes and an npm filename (important for Windows tool path limits).
  const directory = join(config.cache, createHash("sha256").update(JSON.stringify([
    manifest.target, manifest.version, manifest.coreId, component.integrity,
  ])).digest("hex"));
  mkdirSync(directory, { recursive: true });
  const cached = join(directory, "archive.tgz");
  if (existsSync(cached)) { verifyArchive(cached, component); return cached; }
  if (config.offline) throw new Error(`offline plugin cache miss: ${component.package}`);
  const temporary = mkdtempSync(join(directory, ".download-"));
  try {
    const path = fetch(component, temporary);
    verifyArchive(path, component);
    // Concurrent installs can share the immutable content-addressed archive.
    if (!existsSync(cached)) {
      try { renameSync(path, cached); }
      catch (error) { if (!existsSync(cached)) throw error; }
    }
    verifyArchive(cached, component);
    return cached;
  } finally { rmSync(temporary, { recursive: true, force: true }); }
}

function archiveJson(archive, name) {
  const result = spawnSync("tar", ["-xOf", archive, `package/${name}`], {
    encoding: "utf8", timeout: 60000, maxBuffer: 1024 * 1024, windowsHide: true,
  });
  if (result.error || result.status !== 0) throw new Error(`cannot read ${name} from plugin archive`);
  return JSON.parse(result.stdout);
}

function extractPayload(archive, component, manifest, destination) {
  // Only read the pinned regular payload into our own destination; never
  // extract archive paths, links, permissions or lifecycle scripts onto disk.
  const metadata = archiveJson(archive, "package.json");
  const checksums = archiveJson(archive, "checksums.json");
  if (metadata.name !== component.package || metadata.version !== manifest.version ||
      checksums.schemaVersion !== 1 || checksums.target !== manifest.target ||
      checksums.component !== component.package ||
      JSON.stringify(Object.keys(checksums.files || {})) !== JSON.stringify([component.payload]) ||
      checksums.files[component.payload].sha256 !== component.checksum.sha256 ||
      checksums.files[component.payload].bytes !== component.checksum.bytes) {
    throw new Error(`plugin payload metadata mismatch: ${component.package}`);
  }
  const fd = openSync(destination, "wx", 0o644);
  try {
    const result = spawnSync("tar", ["-xOf", archive, `package/${component.payload}`], {
      stdio: ["ignore", fd, "pipe"], timeout: 180000, maxBuffer: 64 * 1024, windowsHide: true,
    });
    if (result.error || result.status !== 0) throw new Error(`cannot extract plugin payload: ${component.package}`);
  } finally { closeSync(fd); }
  if (lstatSync(destination).size !== component.checksum.bytes || hashFile(destination) !== component.checksum.sha256) {
    throw new Error(`plugin payload checksum mismatch: ${component.package}`);
  }
}

module.exports = { coreIdentity, packageFilename, validHash, validatePlugins, settings,
  detectNvidia, componentArchive, extractPayload, hashFile, verifyArchive };
