import { access, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { createReadStream } from "node:fs";
import { spawn } from "node:child_process";
import { createServer as createHttpServer } from "node:http";
import { createServer } from "node:net";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { homedir } from "node:os";
import {
  defaultReleaseVariant,
  nativePackageEntries,
  npmEnvironment,
  run,
  selectPlatform,
  sha256File,
  workspaceDir,
} from "./npm-package-core.mjs";
import { checkPublishedPackage } from "./npm-release-registry.mjs";
import { resolvePlatformRoot, verifyInstallation } from "../npm/meta/install.cjs";
import { installedCliEnvironment, runInstalledCli } from "./npm-installed-cli.mjs";
import { smokeInstalledModels } from "./npm-model-smoke.mjs";

export function installedPackageRoot(prefix, packageName) {
  const modules =
    process.platform === "win32"
      ? join(prefix, "node_modules")
      : join(prefix, "lib", "node_modules");
  return join(modules, ...packageName.split("/"));
}

export async function verifyInstalledRelease({ manifest, entry, version, prefix, requireCuda = false }) {
  const metaRoot = installedPackageRoot(prefix, manifest.metaPackage);
  // Registry-only meta installs can nest dependencies; artifact installs may
  // put them beside meta. Resolve exactly as the real installer does.
  const platformRoot = resolvePlatformRoot(metaRoot, entry);
  for (const [root, name] of [[metaRoot, manifest.metaPackage], [platformRoot, entry.package]]) {
    const metadata = JSON.parse(await readFile(join(root, "package.json"), "utf8"));
    if (metadata.name !== name || metadata.version !== version ||
        (name === entry.package && metadata.zvecGrep?.variant !== defaultReleaseVariant(entry))) {
      throw new Error(`installed artifact identity or variant mismatch: ${name}`);
    }
  }
  return verifyInstallation({ metaRoot, platformRoot, entry, expectedVersion: version, requireCuda });
}

async function availablePort() {
  const server = createServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = server.address().port;
  await new Promise((resolve, reject) =>
    server.close((error) => (error ? reject(error) : resolve())),
  );
  return port;
}

export async function smokeInstalledPackage({
  manifest,
  entry,
  version,
  prefix,
  fixtureRoot,
}) {
  const installedMeta = installedPackageRoot(prefix, manifest.metaPackage);
  const installedPlatform = resolvePlatformRoot(installedMeta, entry);
  const binary = join(installedMeta, "bin", "zg.exe");
  if (
    (await sha256File(binary)) !==
    (await sha256File(join(installedPlatform, "bin", entry.binary)))
  ) {
    throw new Error(
      "materialized meta binary does not match the platform package",
    );
  }
  await rm(fixtureRoot, { recursive: true, force: true });
  await mkdir(fixtureRoot, { recursive: true });
  await writeFile(
    join(fixtureRoot, "fixture.txt"),
    "rust-release-smoke-marker\n",
  );
  let commandIndex = 0;
  const execute = async (args, mode = "direct") =>
    (await runInstalledCli(prefix, args, {
      cwd: fixtureRoot,
      logRoot: join(fixtureRoot, "logs"),
      label: `cli-${commandIndex++}-${args[0].replace(/^--/, "")}`,
      env: installedCliEnvironment(prefix, join(fixtureRoot, "home"), {
        ZVEC_GREP_MODE: mode,
      }),
    })).stdout;
  if ((await execute(["--version"])).trim() !== version) {
    throw new Error(`installed zg version does not match ${version}`);
  }
  if (
    !(await execute([
      "--rg",
      "-F",
      "rust-release-smoke-marker",
      "fixture.txt",
    ])).includes("rust-release-smoke-marker")
  ) {
    throw new Error("packaged zg managed-rg search failed");
  }
  const direct = await execute(["--status", "--no-color"]);
  try {
    await execute([
      "--server",
      "on",
      "--listen",
      `127.0.0.1:${await availablePort()}`,
    ]);
    const status = await execute(["--server", "status", "--check-ready"]);
    if (
      !status.includes("Server: ready") ||
      !status.includes("PID:") ||
      !status.includes("URL:")
    ) {
      throw new Error("packaged daemon did not become ready");
    }
    // Unlike managed --rg, workspace status in server mode crosses the IPC
    // boundary and exercises the packaged daemon's command endpoint.
    const server = await execute(["--status", "--no-color"], "server");
    if (direct !== server)
      throw new Error("packaged direct/server workspace status differs");
  } finally {
    await execute(["--server", "off"]);
  }
  if (!(await execute(["--server", "status"])).includes("Server: stopped")) {
    throw new Error("packaged daemon did not stop");
  }
  let unavailable = false;
  try {
    await execute(["--status", "--no-color"], "server");
  } catch {
    unavailable = true;
  }
  if (!unavailable)
    throw new Error("server status unexpectedly succeeded without the daemon");
  console.log(
    `release smoke passed: zg ${version} (${entry.target}), managed-rg and daemon IPC`,
  );
}

// npm's postinstall invokes npm pack synchronously. Keep this parent asynchronous
// so the loopback registry can serve the actual, unpublished component archives.
function runArtifactNpm(command, args, options) {
  const windows = process.platform === "win32";
  const npmCli = process.env.npm_execpath ??
    join(dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js");
  return new Promise((resolve, reject) => {
    const child = spawn(windows ? process.execPath : command, windows ? [npmCli, ...args] : args,
      { cwd: workspaceDir, env: options.env, stdio: "inherit", timeout: 600_000 });
    child.on("error", reject);
    child.on("close", (code, signal) => code === 0 ? resolve() :
      reject(new Error(`artifact npm ${args[0]} failed: ${signal || code}`)));
  });
}

async function withArtifactRegistry({ release, entry, tarballRoot, evidence }, operation) {
  const components = release.packages.filter((pkg) =>
    entry.components?.some((component) => component.package === pkg.name));
  const requests = [];
  const server = createHttpServer((request, response) => {
    let path;
    try { path = decodeURIComponent(new URL(request.url, "http://localhost").pathname); }
    catch { response.writeHead(400).end(); return; }
    const pkg = components.find((candidate) =>
      path === `/${candidate.name}` || path === `/tarballs/${candidate.file}`);
    requests.push({ path, component: pkg?.name ?? null });
    // No proxy/public registry fallback, nor any files outside the release inventory.
    if (!pkg) { response.writeHead(404).end(); return; }
    if (path.startsWith("/tarballs/")) {
      response.setHeader("Content-Type", "application/octet-stream");
      createReadStream(join(tarballRoot, pkg.file)).on("error", () => response.destroy()).pipe(response);
      return;
    }
    response.setHeader("Content-Type", "application/json");
    response.end(JSON.stringify({ name: pkg.name, "dist-tags": { latest: pkg.version }, versions: {
      [pkg.version]: { name: pkg.name, version: pkg.version, dist: {
        tarball: `http://127.0.0.1:${server.address().port}/tarballs/${pkg.file}`, integrity: pkg.integrity,
      } },
    } }));
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  try {
    return await operation(`http://127.0.0.1:${server.address().port}`, requests);
  } finally {
    await new Promise((resolve) => server.close(resolve));
    await writeFile(evidence, `${JSON.stringify({ requests,
      cudaRequests: requests.filter((request) => request.component).length }, null, 2)}\n`);
  }
}

function artifactEnvironment(prefixRoot, accelerator, registry, tarballRoot) {
  const env = { ...npmEnvironment(join(prefixRoot, "cache")),
    ZG_PLUGIN_DIR: registry ? "" : tarballRoot,
    ZG_PLUGIN_CACHE: join(prefixRoot, "plugin-cache"),
    ZG_INSTALL_OFFLINE: registry ? "0" : "1",
    npm_config_offline: registry ? "false" : "true" };
  // Exercise the installer's real default, not an explicit CPU/auto override.
  if (accelerator === "auto") delete env.ZG_INSTALL_ACCELERATOR;
  else env.ZG_INSTALL_ACCELERATOR = accelerator;
  if (registry) {
    env.npm_config_registry = registry;
    env["npm_config_@zvec:registry"] = registry;
  }
  return env;
}

async function artifactInputs({ manifest, release, tarballRoot, target, accelerator, expectCpu }) {
  if (
    release.schemaVersion !== 1 || !Array.isArray(release.packages) ||
    !/^\d+\.\d+\.\d+(?:-[\w.-]+)?(?:\+[\w.-]+)?$/.test(release.version)
  ) {
    throw new Error("invalid artifact release manifest");
  }
  const entry = selectPlatform(
    manifest,
    process.platform,
    process.arch,
    process.report?.getReport().header ?? {},
  );
  if (target && target !== entry.target) throw new Error(`artifact target ${target} does not match host ${entry.target}`);
  if (!["cpu", "auto", "cuda"].includes(accelerator)) throw new Error("invalid installation accelerator");
  const tarballs = [];
  for (const { name, target, component } of [
    ...nativePackageEntries(entry),
    { name: manifest.metaPackage, target: "meta" },
  ]) {
    if (component && accelerator === "cpu" && !expectCpu) continue;
    const matches = release.packages.filter((candidate) => candidate.name === name);
    const candidate = matches[0];
    const file = `${name.replace(/^@/, "").replaceAll("/", "-")}-${release.version}.tgz`;
    if (
      matches.length !== 1 || candidate.version !== release.version ||
      candidate.target !== target || candidate.file !== file
    ) {
      throw new Error(`artifact package identity mismatch: ${name}`);
    }
    const path = join(tarballRoot, file);
    if ((await sha256File(path)) !== candidate.sha256) {
      throw new Error(`artifact tarball checksum mismatch: ${file}`);
    }
    if (!component) tarballs.push(path);
  }
  return { entry, tarballs };
}

async function assertCpuInstallation({ installation, entry, prefixRoot, bundle }) {
  if (installation.components.length) throw new Error("CPU artifact installation unexpectedly installed CUDA plugins");
  for (const file of new Set((entry.components ?? []).map((component) => component.file))) {
    const exists = await access(join(bundle, file)).then(() => true, (error) => {
      if (error.code === "ENOENT") return false;
      throw error;
    });
    if (exists) throw new Error(`CPU artifact installation contains a CUDA plugin: ${file}`);
  }
  const cache = await readdir(join(prefixRoot, "plugin-cache")).catch((error) => {
    if (error.code === "ENOENT") return [];
    throw error;
  });
  if (cache.length) throw new Error("CPU artifact installation attempted to populate the CUDA cache");
}

export async function assertNoRegistryCudaDownloads(prefixRoot, entry) {
  const directory = join(prefixRoot, "cache", "_logs");
  const names = await readdir(directory).catch((error) => {
    if (error.code === "ENOENT") return [];
    throw error;
  });
  const requests = [];
  for (const name of names.filter((name) => name.endsWith(".log"))) {
    const contents = await readFile(join(directory, name), "utf8");
    for (const line of contents.split("\n")) {
      if (!/http fetch|verbose title npm pack/i.test(line)) continue;
      let decoded = line;
      try { decoded = decodeURIComponent(line); } catch { /* Keep non-URL log text. */ }
      if ((entry.components ?? []).some((component) => decoded.toLowerCase().includes(component.package.toLowerCase()))) {
        requests.push({ log: name, line });
      }
    }
  }
  await writeFile(join(prefixRoot, "cpu-downloads.json"), `${JSON.stringify({ checkedLogs: names, requests }, null, 2)}\n`);
  if (requests.length) throw new Error("Default registry CPU installation requested CUDA components");
}

export async function installArtifactRelease({
  manifest, release, tarballRoot, prefixRoot, target,
  accelerator = "auto", expectCpu = false, expectCuda = false,
  runNpm = runArtifactNpm, verifyInstalled = verifyInstalledRelease,
}) {
  if (expectCpu && accelerator !== "auto") throw new Error("CPU-host verification must use default auto installation");
  if (expectCuda && (expectCpu || accelerator === "cpu")) throw new Error("CUDA verification conflicts with CPU installation/verification");
  const { entry, tarballs } = await artifactInputs({ manifest, release, tarballRoot, target, accelerator, expectCpu });
  if (expectCuda && !entry.components?.length) throw new Error("CUDA verification requires a platform with CUDA components");
  // Use the exact downloaded bytes, a fresh prefix/cache and no registry fallback.
  // Verification must never build or repack a package.
  const destination = resolve(prefixRoot);
  const artifacts = relative(destination, resolve(tarballRoot));
  if (destination === dirname(destination) || destination === resolve(homedir()) ||
      destination === workspaceDir || (!artifacts.startsWith("..") && !isAbsolute(artifacts))) {
    throw new Error("artifact installation needs a dedicated prefix directory outside its inputs");
  }
  const existing = await readdir(destination).catch((error) => { if (error.code === "ENOENT") return []; throw error; });
  if (existing.some((name) => !["prefix", "cache", "plugin-cache", "workspace", "installation.json",
    "cpu-installation.json", "cpu-downloads.json", "rebuild-downloads.json"].includes(name))) {
    throw new Error("refusing to replace a directory not owned by artifact installation");
  }
  await rm(prefixRoot, { recursive: true, force: true });
  await mkdir(prefixRoot, { recursive: true });
  const prefix = join(prefixRoot, "prefix");
  const install = (registry) => runNpm(
    "npm",
    [
      "install", "--global", "--prefix", prefix,
      ...(registry ? [`--registry=${registry}`, `--@zvec:registry=${registry}`, "--fetch-retries=0"] : ["--offline"]),
      "--no-audit", "--no-fund", "--ignore-scripts=false", "--foreground-scripts",
      `--allow-scripts=${tarballs.at(-1)}`, ...tarballs,
    ],
    { env: artifactEnvironment(prefixRoot, accelerator, registry, tarballRoot) },
  );
  if (expectCpu) {
    await withArtifactRegistry({ release, entry, tarballRoot, evidence: join(prefixRoot, "cpu-downloads.json") },
      async (registry, requests) => {
        await install(registry);
        if (requests.some((request) => request.component)) {
          throw new Error("Default CPU-host installation requested CUDA plugins (including failed attempts)");
        }
      });
  } else await install();
  const installation = await verifyInstalled({ manifest, entry, version: release.version, prefix,
    requireCuda: expectCuda || accelerator === "cuda" });
  if (accelerator === "cpu" && installation.coreId && installation.components.length) {
    throw new Error("CPU artifact installation unexpectedly installed CUDA plugins");
  }
  const bundle = installedPackageRoot(prefix, manifest.metaPackage);
  if (expectCpu) await assertCpuInstallation({ installation, entry, prefixRoot, bundle });
  await writeFile(join(prefixRoot, "installation.json"), `${JSON.stringify({
    ...installation, bundle, accelerator, expectCpu, expectCuda, commit: release.commit,
    packages: release.packages.filter((pkg) => pkg.target === entry.target || pkg.target === "meta"),
  }, null, 2)}\n`);
  return { entry, prefix, bundle };
}

export async function rebuildArtifactRelease({
  manifest, release, tarballRoot, prefixRoot, target,
  runNpm = runArtifactNpm, verifyInstalled = verifyInstalledRelease,
}) {
  const { entry } = await artifactInputs({ manifest, release, tarballRoot, target, accelerator: "cuda" });
  if (!entry.components?.length) throw new Error("CUDA rebuild requires a platform with CUDA components");
  const prefix = join(prefixRoot, "prefix");
  const bundle = installedPackageRoot(prefix, manifest.metaPackage);
  const report = JSON.parse(await readFile(join(prefixRoot, "installation.json"), "utf8"));
  const packages = release.packages.filter((pkg) => pkg.target === entry.target || pkg.target === "meta");
  if (report.commit !== release.commit || JSON.stringify(report.packages) !== JSON.stringify(packages)) {
    throw new Error("CPU installation belongs to a different artifact release");
  }
  const before = await verifyInstalled({ manifest, entry, version: release.version, prefix });
  if (before.coreId !== report.coreId || before.target !== report.target || before.version !== report.version) {
    throw new Error("CPU installation no longer matches its original receipt");
  }
  await assertCpuInstallation({ installation: before, entry, prefixRoot, bundle });
  const binarySha256 = await sha256File(join(bundle, "bin/zg.exe"));
  await writeFile(join(prefixRoot, "cpu-installation.json"), `${JSON.stringify(report, null, 2)}\n`);
  await withArtifactRegistry({ release, entry, tarballRoot, evidence: join(prefixRoot, "rebuild-downloads.json") },
    async (registry, requests) => {
      // Deliberately no rm, npm install or fresh prefix: rerun the installed postinstall.
      await runNpm("npm", ["rebuild", "--global", "--prefix", prefix,
        `--registry=${registry}`, `--@zvec:registry=${registry}`,
        "--ignore-scripts=false", `--allow-scripts=${manifest.metaPackage}`, "--foreground-scripts",
        manifest.metaPackage], { env: artifactEnvironment(prefixRoot, "cuda", registry) });
      for (const component of entry.components) {
        const pkg = packages.find((candidate) => candidate.name === component.package);
        if (!requests.some((request) => request.path === `/tarballs/${pkg.file}`)) {
          throw new Error(`npm rebuild did not fetch this run's CUDA component: ${pkg.name}`);
        }
      }
    });
  const after = await verifyInstalled({ manifest, entry, version: release.version, prefix, requireCuda: true });
  if (!before.coreId || after.coreId !== before.coreId ||
      await sha256File(join(bundle, "bin/zg.exe")) !== binarySha256) {
    throw new Error("npm rebuild changed the installed CPU core or executable");
  }
  await writeFile(join(prefixRoot, "installation.json"), `${JSON.stringify({
    ...after, bundle, accelerator: "cuda", operation: "rebuild", commit: release.commit,
    binarySha256, previousCoreId: before.coreId, packages,
  }, null, 2)}\n`);
  return { entry, prefix, bundle };
}

export async function smokeArtifactRelease(options) {
  const result = await installArtifactRelease(options);
  await (options.probe ?? smokeInstalledPackage)({
    manifest: options.manifest,
    entry: result.entry,
    version: options.release.version,
    prefix: result.prefix,
    fixtureRoot: join(options.prefixRoot, "workspace"),
  });
  return result;
}

export async function rollbackVersion(
  manifest,
  candidate,
  registryFetch = fetch,
) {
  const response = await registryFetch(
    `https://registry.npmjs.org/${encodeURIComponent(manifest.metaPackage)}`,
    { signal: AbortSignal.timeout(30_000) },
  );
  if (!response.ok)
    throw new Error(
      `cannot determine rollback version: registry returned ${response.status}`,
    );
  const metadata = await response.json();
  const latest = metadata["dist-tags"]?.latest;
  const previous =
    latest !== candidate
      ? latest
      : Object.keys(metadata.versions ?? {})
          .filter((version) => version !== candidate)
          .sort((a, b) =>
            (metadata.time?.[b] ?? "").localeCompare(metadata.time?.[a] ?? ""),
          )[0];
  if (
    !previous ||
    previous === candidate ||
    !/^\d+\.\d+\.\d+(?:-[\w.-]+)?$/.test(previous)
  ) {
    throw new Error(
      "registry update/rollback smoke requires a distinct published baseline version",
    );
  }
  return previous;
}

export async function smokeRegistryRelease({
  manifest,
  release,
  baseline,
  prefixRoot,
  registryFetch = fetch,
  runNpm = run,
  probe = smokeInstalledPackage,
  modelProbe = smokeInstalledModels,
  probePrevious,
  verifyInstalled = verifyInstalledRelease,
}) {
  if (
    !/^\d+\.\d+\.\d+(?:-[\w.-]+)?$/.test(baseline ?? "") ||
    baseline === release.version
  ) {
    throw new Error(
      "registry update/rollback smoke requires a distinct baseline version",
    );
  }
  for (const entry of release.packages) {
    if (!(await checkPublishedPackage(entry, registryFetch)))
      throw new Error(
        `registry candidate is missing: ${entry.name}@${entry.version}`,
      );
  }
  const entry = selectPlatform(
    manifest,
    process.platform,
    process.arch,
    process.report?.getReport().header ?? {},
  );
  const environment = { ...npmEnvironment(join(prefixRoot, "cache")),
    ZG_PLUGIN_DIR: "", ZG_PLUGIN_CACHE: join(prefixRoot, "plugin-cache"), ZG_INSTALL_OFFLINE: "0" };
  delete environment.ZG_INSTALL_ACCELERATOR;
  environment.ZG_INSTALL_SKIP_AGENT_UPGRADE = "1";
  // Resolve optional dependencies from the public registry, without installing
  // a platform tarball explicitly or reusing a local candidate's npm cache.
  const install = (prefix, version) =>
    runNpm(
      "npm",
      [
        "install",
        "--global",
        "--prefix",
        prefix,
        "--registry=https://registry.npmjs.org",
        "--no-audit",
        "--no-fund",
        `--allow-scripts=${manifest.metaPackage}`,
        `${manifest.metaPackage}@${version}`,
      ],
      { env: environment },
    );
  const verifyPrevious =
    probePrevious ??
    (async (prefix) => {
      const root = installedPackageRoot(prefix, manifest.metaPackage);
      const metadata = JSON.parse(
        await readFile(join(root, "package.json"), "utf8"),
      );
      if (metadata.version !== baseline)
        throw new Error("rollback package version differs from the baseline");
      if ((await runInstalledCli(prefix, ["--version"], {
        env: installedCliEnvironment(prefix, join(prefixRoot, "baseline-home")),
      })).stdout.trim() !== baseline)
        throw new Error(
          "rollback executable version differs from the baseline",
        );
    });
  const fresh = join(prefixRoot, "fresh");
  const update = join(prefixRoot, "update");
  await rm(prefixRoot, { recursive: true, force: true });
  await install(fresh, release.version);
  const installation = await verifyInstalled({ manifest, entry, version: release.version, prefix: fresh });
  await assertCpuInstallation({ installation, entry, prefixRoot,
    bundle: installedPackageRoot(fresh, manifest.metaPackage) });
  await assertNoRegistryCudaDownloads(prefixRoot, entry);
  await writeFile(join(prefixRoot, "installation.json"), `${JSON.stringify(installation, null, 2)}\n`);
  await probe({
    manifest,
    entry,
    version: release.version,
    prefix: fresh,
    fixtureRoot: join(prefixRoot, "fresh-workspace"),
  });
  await modelProbe({ prefix: fresh, fixtureRoot: join(prefixRoot, "fresh-models") });
  await install(update, baseline);
  await verifyPrevious(update);
  await install(update, release.version);
  const upgraded = await verifyInstalled({ manifest, entry, version: release.version, prefix: update });
  await writeFile(join(prefixRoot, "update-installation.json"), `${JSON.stringify(upgraded, null, 2)}\n`);
  await assertCpuInstallation({ installation: upgraded, entry, prefixRoot,
    bundle: installedPackageRoot(update, manifest.metaPackage) });
  await assertNoRegistryCudaDownloads(prefixRoot, entry);
  await probe({
    manifest,
    entry,
    version: release.version,
    prefix: update,
    fixtureRoot: join(prefixRoot, "update-workspace"),
  });
  await modelProbe({ prefix: update, fixtureRoot: join(prefixRoot, "update-models") });
  await install(update, baseline);
  await verifyPrevious(update);
  await assertNoRegistryCudaDownloads(prefixRoot, entry);
  if (entry.components?.length) {
    // Run explicit CUDA verification only after all default CPU installations,
    // with separate caches so it cannot mask unexpected upgrade downloads.
    await runNpm("npm", ["rebuild", "--global", "--prefix", fresh,
      "--registry=https://registry.npmjs.org", `--allow-scripts=${manifest.metaPackage}`, manifest.metaPackage, "--foreground-scripts"],
    { env: { ...environment, ZG_INSTALL_ACCELERATOR: "cuda",
      npm_config_cache: join(prefixRoot, "cuda-cache"), ZG_PLUGIN_CACHE: join(prefixRoot, "cuda-plugin-cache") } });
    await verifyInstalled({ manifest, entry, version: release.version, prefix: fresh, requireCuda: true });
  }
  console.log(
    `registry install/update/rollback passed: ${baseline} -> ${release.version} -> ${baseline} (${entry.target})`,
  );
  await writeFile(join(prefixRoot, "results.json"), `${JSON.stringify({
    target: entry.target, version: release.version, commit: release.commit, baseline,
    passed: true, entry: "npm zg", models: ["potion", "gguf", "onnx"],
  }, null, 2)}\n`);
}
