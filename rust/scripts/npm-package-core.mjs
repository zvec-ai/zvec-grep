import { createHash } from "node:crypto";
import { createReadStream, createWriteStream, readFileSync } from "node:fs";
import { copyFile, mkdir, readFile, readdir, stat, writeFile } from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { pipeline } from "node:stream/promises";

const scriptPath = fileURLToPath(import.meta.url);
export const workspaceDir = resolve(dirname(scriptPath), "..");
export const npmManifestPath = join(workspaceDir, "npm", "platforms.json");
// npm embeds tarballs as base64 in a JSON string. Stay comfortably below the
// ~384 MiB tarball ceiling imposed by 64-bit Node's ~512 MiB string limit.
export const MAX_NPM_TARBALL_BYTES = 350 * 1024 * 1024;

export async function verifyTarballSize(path) {
  const { size } = await stat(path);
  if (size > MAX_NPM_TARBALL_BYTES) {
    throw new Error(`npm tarball exceeds 350 MiB publication budget: ${path} (${size} bytes); split the payload before publishing`);
  }
  return size;
}

// Pinned plugins must be published before their platform package; meta is last.
export function nativePackageEntries(entry) {
  return [
    ...(entry.components ?? []).map((component) => ({
      name: component.package, target: entry.target, component: component.package,
    })),
    { name: entry.package, target: entry.target },
  ];
}

// Split by byte range, not by loading a potentially >512 MiB ORT library into
// memory. Reassembly is byte-for-byte and verified against the original hash.
export async function stageComponentPayload(sourceRoot, destinationRoot, entry, component) {
  const siblings = entry.components.filter((candidate) => candidate.file === component.file);
  const index = siblings.indexOf(component);
  if (index < 0) throw new Error("unknown native component");
  const source = join(sourceRoot, component.file);
  const metadata = await stat(source);
  if (!metadata.isFile() || metadata.size < siblings.length) {
    throw new Error(`invalid component source: ${source}`);
  }
  const start = Math.floor(metadata.size * index / siblings.length);
  const end = Math.floor(metadata.size * (index + 1) / siblings.length) - 1;
  const destination = join(destinationRoot, component.payload);
  await mkdir(dirname(destination), { recursive: true });
  await pipeline(createReadStream(source, { start, end }), createWriteStream(destination, { mode: metadata.mode }));
  return { bytes: end - start + 1, sha256: await sha256File(destination) };
}

export function loadReleaseManifest() {
  const manifest = JSON.parse(readFileSync(npmManifestPath, "utf8"));
  if (
    manifest.schemaVersion !== 1 ||
    typeof manifest.metaPackage !== "string" ||
    typeof manifest.nodeEngine !== "string"
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
    if (
      entry.defaultVariant !== undefined
      && !["cpu", "cuda"].includes(entry.defaultVariant)
    ) {
      throw new Error(`unsupported default npm variant for ${entry.target}`);
    }
    if (targets.has(entry.target)) {
      throw new Error(`duplicate npm target: ${entry.target}`);
    }
    if (packages.has(entry.package)) {
      throw new Error(`duplicate npm platform package: ${entry.package}`);
    }
    targets.add(entry.target);
    packages.add(entry.package);
    if (entry.components !== undefined && (!Array.isArray(entry.components) || entry.defaultVariant !== "cuda")) {
      throw new Error(`invalid native components for ${entry.target}`);
    }
    for (const component of entry.components ?? []) {
      if (typeof component.package !== "string" || !component.package.startsWith(`${entry.package}-`) ||
          !/^@[a-z0-9-]+\/[a-z0-9-]+$/.test(component.package) || packages.has(component.package) ||
          !/^lib\/[a-zA-Z0-9_./-]+$/.test(component.file) ||
          !/^(lib|parts)\/[a-zA-Z0-9_./-]+$/.test(component.payload) ||
          [component.file, component.payload].some((path) => path.split("/").some((part) => !part || part === "." || part === ".."))) {
        throw new Error(`invalid or duplicate native component for ${entry.target}`);
      }
      packages.add(component.package);
    }
  }
  return manifest;
}

export function selectPlatform(manifest, platform, arch, reportHeader = {}) {
  const libc =
    platform === "linux"
      ? reportHeader.glibcVersionRuntime
        ? "glibc"
        : "musl"
      : undefined;
  const matches = manifest.platforms.filter(
    (entry) =>
      entry.os === platform &&
      entry.cpu === arch &&
      (entry.libc === undefined || entry.libc === libc),
  );
  if (matches.length !== 1) {
    const suffix = libc ? `-${libc}` : "";
    throw new Error(`unsupported npm platform: ${platform}-${arch}${suffix}`);
  }
  return matches[0];
}

export function platformByTarget(manifest, target) {
  const entry = manifest.platforms.find(
    (candidate) => candidate.target === target,
  );
  if (!entry) {
    throw new Error(`unknown npm release target: ${target}`);
  }
  return entry;
}

export function defaultReleaseVariant(entry) {
  return entry.defaultVariant ?? "cpu";
}

export function cargoTargetDirectory() {
  return resolve(workspaceDir, process.env.CARGO_TARGET_DIR ?? "target");
}

export function resolveBuildVariant(entry, cpuOnly, probe = () => run("nvcc", ["--version"], { capture: true }),
  buildMode = process.env.LLAMA_CPP_BUILD_MODE) {
  if (cpuOnly || defaultReleaseVariant(entry) !== "cuda") return "cpu";
  // Explicit SDK mode needs a Rust linker, not an installed CUDA compiler.
  // Auto mode retains the existing no-toolchain => CPU-only behavior.
  if (buildMode === "prebuilt") return "cuda";
  let output;
  try {
    output = probe();
  } catch (error) {
    if (error.code === "ENOENT") return "cpu";
    throw error;
  }
  if (!/release\s+13\.1(?:,|\s)/.test(output)) {
    throw new Error("CUDA packaging requires nvcc 13.1; install it or select --cpu-only");
  }
  return "cuda";
}

export async function buildNative(entry, { cpuOnly = false, runCommand = run } = {}) {
  const variant = resolveBuildVariant(entry, cpuOnly,
    () => runCommand("nvcc", ["--version"], { capture: true }));
  const root = cargoTargetDirectory();
  // CUDA and CPU must never reuse staged native libraries from one another.
  const target = defaultReleaseVariant(entry) === "cuda" ? join(root, variant) : root;
  const env = { ...process.env, CARGO_TARGET_DIR: target };
  // Invalidate the old success record before any command can clean or overwrite
  // its outputs. A failed/interrupted rebuild must not enable --no-build reuse.
  await mkdir(root, { recursive: true });
  await writeFile(join(root, "native-build.json"), `${JSON.stringify({
    target: entry.target, variant, status: "building",
  })}\n`);
  if (variant === "cuda") {
    if (entry.os === "linux") {
      runCommand("bash", ["scripts/cuda/build.sh"], { env });
    } else {
      runCommand("powershell.exe", ["-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "scripts/cuda/build.ps1"], { env });
    }
  } else {
    if (!cpuOnly && defaultReleaseVariant(entry) === "cuda") {
      console.log("CUDA toolkit not found; building CPU-only native artifacts");
    }
    runCommand("cargo", ["build", "--locked", "--release", "--package", "zg"], { env });
  }
  const build = {
    target: entry.target,
    variant,
    profileDir: join(target, "release"),
    ...(variant === "cuda" ? { bundleDir: join(target, "release", "zg-cuda") } : {}),
  };
  await writeFile(join(root, "native-build.json"), `${JSON.stringify(build, null, 2)}\n`);
  return build;
}

export async function existingNativeBuild(entry, { cpuOnly = false } = {}) {
  let build;
  try {
    build = JSON.parse(await readFile(join(cargoTargetDirectory(), "native-build.json"), "utf8"));
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
  if (build?.status === "building") {
    throw new Error("The previous native build did not complete; rebuild successfully before using --no-build");
  }
  if (build?.target === entry.target && (!cpuOnly || build.variant === "cpu")) return build;
  if (cpuOnly && defaultReleaseVariant(entry) === "cuda") {
    return { variant: "cpu", profileDir: join(cargoTargetDirectory(), "cpu", "release") };
  }
  // Plain cargo build remains a supported CPU development entry point.
  return { variant: "cpu", profileDir: join(cargoTargetDirectory(), "release") };
}

export function run(command, args, options = {}) {
  // Windows cannot execute npm.cmd with CreateProcess. Invoke npm's JavaScript
  // entry point directly instead of introducing a shell for package arguments.
  const npmCli =
    process.env.npm_execpath ??
    join(dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js");
  const useNpmCli = process.platform === "win32" && command === "npm";
  const result = spawnSync(
    useNpmCli ? process.execPath : command,
    useNpmCli ? [npmCli, ...args] : args,
    {
      cwd: options.cwd ?? workspaceDir,
      encoding: "utf8",
      timeout: options.timeout,
      stdio: options.capture ? "pipe" : "inherit",
      env: options.env ?? process.env,
    },
  );
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

export async function hashFile(path, algorithm = "sha256", encoding = "hex") {
  const hash = createHash(algorithm);
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest(encoding);
}

export async function sha256File(path) {
  return hashFile(path);
}

// Stage the non-bundle (CPU) runtime below lib/. Windows uses the native
// delay-load hook; Unix binaries use ../lib rpaths. CUDA uses stageRuntimeBundle.
export async function stageZvecRuntime(sourceDir, libDir, platform) {
  const library = {
    darwin: "libzvec_c_api.dylib",
    linux: "libzvec_c_api.so",
    win32: "zvec_c_api.dll",
  }[platform];
  if (!library)
    throw new Error(`unsupported zvec runtime platform: ${platform}`);
  const files = [library];
  for (const file of files) {
    const source = join(sourceDir, file);
    if (!(await stat(source)).isFile())
      throw new Error(`missing zvec runtime file: ${source}`);
  }
  // Cargo also stages model backend libraries beside the executable. Include
  // these in the payload so installation does not depend on the build tree.
  const runtimePattern = {
    darwin: /\.dylib$/,
    linux: /\.so(?:\.\d+)*$/,
    win32: /\.dll$/i,
  }[platform];
  // A plain Cargo profile can retain CUDA outputs after a CPU rebuild, even
  // without native-build.json. CPU llama.cpp is static on these platforms;
  // only zvec and the CPU/DirectML ORT distribution belong in this path.
  // Reject, rather than silently filter, an ambiguous directory: the binary
  // might still require those libraries. Validate before copying any payload.
  const cpuRuntimePattern = {
    linux: /^lib(?:zvec_c_api|onnxruntime)\.so(?:\.\d+)*$/,
    win32: /^(?:zvec_c_api|onnxruntime|DirectML)\.dll$/i,
  }[platform];
  for (const name of (await readdir(sourceDir)).sort()) {
    if (
      name !== library &&
      runtimePattern.test(name) &&
      (await stat(join(sourceDir, name))).isFile()
    ) {
      if (cpuRuntimePattern && !cpuRuntimePattern.test(name)) {
        throw new Error(
          `refusing CPU-only packaging of unverified runtime library: ${join(sourceDir, name)}; ` +
          "CUDA-to-CPU rebuilds can leave stale libraries. Rebuild with --cpu-only in a fresh CARGO_TARGET_DIR, " +
          "or package an audited CUDA bundle with --bundle-dir; do not reuse a mixed Cargo profile",
        );
      }
      files.push(name);
    }
  }
  for (const file of files) {
    const destination = join(libDir, file);
    await mkdir(dirname(destination), { recursive: true });
    await copyFile(join(sourceDir, file), destination);
  }
  return files.map((file) => `lib/${file}`);
}

async function copyRegularTree(sourceRoot, destinationRoot, directory = sourceRoot) {
  const copied = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const source = join(directory, entry.name);
    const payloadPath = relative(sourceRoot, source).split(sep).join("/");
    const destination = join(destinationRoot, ...payloadPath.split("/"));
    if (entry.isDirectory()) {
      await mkdir(destination, { recursive: true });
      copied.push(...await copyRegularTree(sourceRoot, destinationRoot, source));
      continue;
    }
    if (!entry.isFile()) {
      throw new Error(`native bundle entry is not a regular file: ${source}`);
    }
    await mkdir(dirname(destination), { recursive: true });
    await copyFile(source, destination);
    copied.push(payloadPath);
  }
  return copied;
}

export async function stageRuntimeBundle(sourceDir, destinationRoot, platform, binary) {
  const sourceMetadata = await stat(sourceDir);
  if (!sourceMetadata.isDirectory()) {
    throw new Error(`native bundle is not a directory: ${sourceDir}`);
  }
  const library = {
    darwin: "libzvec_c_api.dylib",
    linux: "libzvec_c_api.so",
    win32: "zvec_c_api.dll",
  }[platform];
  if (!library) {
    throw new Error(`unsupported native bundle platform: ${platform}`);
  }
  const required = [
    `bin/${binary}`,
    `lib/${library}`,
  ];
  const directories = await readdir(sourceDir, { withFileTypes: true });
  if (directories.length !== 2 || directories.some((entry) =>
    !entry.isDirectory() || !["bin", "lib"].includes(entry.name))) {
    throw new Error("native bundle must contain only bin/ and lib/ directories");
  }
  const executables = await readdir(join(sourceDir, "bin"));
  if (executables.length !== 1 || executables[0] !== binary) {
    throw new Error(`native bundle bin/ must contain only ${binary}`);
  }
  for (const file of required) {
    const source = join(sourceDir, file);
    if (!(await stat(source)).isFile()) {
      throw new Error(`native bundle is missing required file: ${source}`);
    }
  }
  await mkdir(destinationRoot, { recursive: true });
  return (await copyRegularTree(sourceDir, destinationRoot)).sort();
}
