import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { copyFile, mkdir, readFile, stat } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const scriptPath = fileURLToPath(import.meta.url);
export const workspaceDir = resolve(dirname(scriptPath), "..");
export const npmManifestPath = join(workspaceDir, "npm", "platforms.json");

export function loadReleaseManifest() {
  const manifest = JSON.parse(readFileSync(npmManifestPath, "utf8"));
  if (
    manifest.schemaVersion !== 1
    || typeof manifest.metaPackage !== "string"
    || typeof manifest.nodeEngine !== "string"
  ) {
    throw new Error("npm/platforms.json has an unsupported schema");
  }
  if (!Array.isArray(manifest.platforms) || manifest.platforms.length === 0) {
    throw new Error("npm/platforms.json must declare at least one platform");
  }

  const targets = new Set();
  const packages = new Set();
  for (const entry of manifest.platforms) {
    for (const field of ["target", "package", "os", "cpu", "binary"]) {
      if (typeof entry[field] !== "string" || entry[field].length === 0) {
        throw new Error(`npm platform entry is missing ${field}`);
      }
    }
    if (targets.has(entry.target)) {
      throw new Error(`duplicate npm target: ${entry.target}`);
    }
    if (packages.has(entry.package)) {
      throw new Error(`duplicate npm platform package: ${entry.package}`);
    }
    targets.add(entry.target);
    packages.add(entry.package);
  }
  return manifest;
}

export function selectPlatform(manifest, platform, arch, reportHeader = {}) {
  const libc = platform === "linux"
    ? (reportHeader.glibcVersionRuntime ? "glibc" : "musl")
    : undefined;
  const matches = manifest.platforms.filter((entry) =>
    entry.os === platform
    && entry.cpu === arch
    && (entry.libc === undefined || entry.libc === libc));
  if (matches.length !== 1) {
    const suffix = libc ? `-${libc}` : "";
    throw new Error(`unsupported npm platform: ${platform}-${arch}${suffix}`);
  }
  return matches[0];
}

export function platformByTarget(manifest, target) {
  const entry = manifest.platforms.find((candidate) => candidate.target === target);
  if (!entry) {
    throw new Error(`unknown npm release target: ${target}`);
  }
  return entry;
}

export function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: options.cwd ?? workspaceDir,
    encoding: "utf8",
    stdio: options.capture ? "pipe" : "inherit",
    env: options.env ?? process.env,
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    const detail = options.capture ? `\n${result.stderr.trim()}` : "";
    throw new Error(`${command} exited with status ${result.status}${detail}`);
  }
  return result.stdout;
}

export function cargoPackageVersion() {
  const stdout = run(
    "cargo",
    ["metadata", "--locked", "--no-deps", "--format-version", "1"],
    { capture: true },
  );
  const metadata = JSON.parse(stdout);
  const zgPackage = metadata.packages.find((entry) => entry.name === "zg");
  if (!zgPackage) {
    throw new Error("cargo metadata did not contain the zg package");
  }
  return zgPackage.version;
}

export function npmEnvironment(cacheDir) {
  return {
    ...process.env,
    npm_config_cache: cacheDir,
    npm_config_update_notifier: "false",
  };
}

export async function sha256File(path) {
  const contents = await readFile(path);
  return createHash("sha256").update(contents).digest("hex");
}

// Preserve the SDK layout relative to the loaded library, including on Windows
// where the DLL must live beside the executable.
export async function stageZvecRuntime(sourceDir, binDir, platform) {
  const library = {
    darwin: "libzvec_c_api.dylib",
    linux: "libzvec_c_api.so",
    win32: "zvec_c_api.dll",
  }[platform];
  if (!library) throw new Error(`unsupported zvec runtime platform: ${platform}`);
  const files = [library, "data/jieba_dict/jieba.dict.utf8", "data/jieba_dict/hmm_model.utf8"];
  for (const file of files) {
    const source = join(sourceDir, file);
    if (!(await stat(source)).isFile()) throw new Error(`missing zvec runtime file: ${source}`);
  }
  for (const file of files) {
    const destination = join(binDir, file);
    await mkdir(dirname(destination), { recursive: true });
    await copyFile(join(sourceDir, file), destination);
  }
  return files.map((file) => `bin/${file}`);
}
