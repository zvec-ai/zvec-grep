import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { copyFile, mkdir, readFile, readdir, stat, writeFile } from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const scriptPath = fileURLToPath(import.meta.url);
export const workspaceDir = resolve(dirname(scriptPath), "..");
export const npmManifestPath = join(workspaceDir, "npm", "platforms.json");

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

export function resolveBuildVariant(entry, cpuOnly, probe = () => run("nvcc", ["--version"], { capture: true })) {
  if (cpuOnly || defaultReleaseVariant(entry) !== "cuda") return "cpu";
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

export async function buildNative(entry, { cpuOnly = false } = {}) {
  const variant = resolveBuildVariant(entry, cpuOnly);
  const root = cargoTargetDirectory();
  // CUDA and CPU must never reuse staged native libraries from one another.
  const target = defaultReleaseVariant(entry) === "cuda" ? join(root, variant) : root;
  const env = { ...process.env, CARGO_TARGET_DIR: target };
  if (variant === "cuda") {
    if (entry.os === "linux") {
      run("bash", ["scripts/cuda/build.sh"], { env });
    } else {
      run("powershell.exe", ["-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "scripts/cuda/build.ps1"], { env });
    }
  } else {
    if (!cpuOnly && defaultReleaseVariant(entry) === "cuda") {
      console.log("CUDA toolkit not found; building CPU-only native artifacts");
    }
    run("cargo", ["build", "--locked", "--release", "--package", "zg"], { env });
  }
  const build = {
    target: entry.target,
    variant,
    profileDir: join(target, "release"),
    ...(variant === "cuda" ? { bundleDir: join(target, "release", "zg-cuda") } : {}),
  };
  await mkdir(root, { recursive: true });
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

export async function sha256File(path) {
  const contents = await readFile(path);
  return createHash("sha256").update(contents).digest("hex");
}

// Keep every runtime library and its data below lib/. Windows uses the native
// delay-load hook; Unix binaries use ../lib rpaths.
export async function stageZvecRuntime(sourceDir, libDir, platform) {
  const library = {
    darwin: "libzvec_c_api.dylib",
    linux: "libzvec_c_api.so",
    win32: "zvec_c_api.dll",
  }[platform];
  if (!library)
    throw new Error(`unsupported zvec runtime platform: ${platform}`);
  const files = [
    library,
    "data/jieba_dict/jieba.dict.utf8",
    "data/jieba_dict/hmm_model.utf8",
  ];
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
  for (const name of (await readdir(sourceDir)).sort()) {
    if (
      name !== library &&
      runtimePattern.test(name) &&
      (await stat(join(sourceDir, name))).isFile()
    ) {
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
    "lib/data/jieba_dict/jieba.dict.utf8",
    "lib/data/jieba_dict/hmm_model.utf8",
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
