import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { join } from "node:path";
import {
  defaultReleaseVariant,
  npmEnvironment,
  run,
  selectPlatform,
  sha256File,
} from "./npm-package-core.mjs";
import { checkPublishedPackage } from "./npm-release-registry.mjs";

export function installedPackageRoot(prefix, packageName) {
  const modules =
    process.platform === "win32"
      ? join(prefix, "node_modules")
      : join(prefix, "lib", "node_modules");
  return join(modules, ...packageName.split("/"));
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
  const installedPlatform = installedPackageRoot(prefix, entry.package);
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
  const execute = (args, mode = "direct") =>
    run(binary, args, {
      capture: true,
      timeout: 30_000,
      cwd: fixtureRoot,
      env: {
        ...process.env,
        ZVEC_GREP_HOME: join(fixtureRoot, "home"),
        ZVEC_GREP_WORKSPACE_REGISTRY: join(fixtureRoot, "workspaces.json"),
        ZVEC_GREP_MODE: mode,
      },
    });
  if (execute(["--version"]).trim() !== version) {
    throw new Error(`installed zg version does not match ${version}`);
  }
  if (
    !execute([
      "--rg",
      "-F",
      "rust-release-smoke-marker",
      "fixture.txt",
    ]).includes("rust-release-smoke-marker")
  ) {
    throw new Error("packaged zg managed-rg search failed");
  }
  const direct = execute(["--status", "--no-color"]);
  try {
    execute([
      "--server",
      "on",
      "--listen",
      `127.0.0.1:${await availablePort()}`,
    ]);
    const status = execute(["--server", "status", "--check-ready"]);
    if (
      !status.includes("Server: ready") ||
      !status.includes("PID:") ||
      !status.includes("URL:")
    ) {
      throw new Error("packaged daemon did not become ready");
    }
    // Unlike managed --rg, workspace status in server mode crosses the IPC
    // boundary and exercises the packaged daemon's command endpoint.
    const server = execute(["--status", "--no-color"], "server");
    if (direct !== server)
      throw new Error("packaged direct/server workspace status differs");
  } finally {
    execute(["--server", "off"]);
  }
  if (!execute(["--server", "status"]).includes("Server: stopped")) {
    throw new Error("packaged daemon did not stop");
  }
  let unavailable = false;
  try {
    execute(["--status", "--no-color"], "server");
  } catch {
    unavailable = true;
  }
  if (!unavailable)
    throw new Error("server status unexpectedly succeeded without the daemon");
  console.log(
    `release smoke passed: zg ${version} (${entry.target}), managed-rg and daemon IPC`,
  );
}

export async function smokeArtifactRelease({
  manifest,
  release,
  tarballRoot,
  prefixRoot,
  runNpm = run,
  probe = smokeInstalledPackage,
}) {
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
  const tarballs = [];
  for (const [name, target] of [
    [entry.package, entry.target],
    [manifest.metaPackage, "meta"],
  ]) {
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
    tarballs.push(path);
  }
  // Use the exact downloaded bytes, a fresh prefix/cache and no registry fallback.
  // Unlike smoke-current, this must never rebuild or repack a native package.
  await rm(prefixRoot, { recursive: true, force: true });
  const prefix = join(prefixRoot, "prefix");
  await runNpm(
    "npm",
    [
      "install", "--global", "--prefix", prefix,
      "--offline", "--no-audit", "--no-fund",
      `--allow-scripts=${tarballs[1]}`, ...tarballs,
    ],
    { env: npmEnvironment(join(prefixRoot, "cache")) },
  );
  const metadata = JSON.parse(
    await readFile(join(installedPackageRoot(prefix, entry.package), "package.json"), "utf8"),
  );
  if (
    metadata.name !== entry.package || metadata.version !== release.version ||
    metadata.zvecGrep?.variant !== defaultReleaseVariant(entry)
  ) {
    throw new Error(`installed artifact identity or variant mismatch: ${entry.target}`);
  }
  await probe({
    manifest,
    entry,
    version: release.version,
    prefix,
    fixtureRoot: join(prefixRoot, "workspace"),
  });
  return { entry, prefix, bundle: installedPackageRoot(prefix, manifest.metaPackage) };
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
  probePrevious,
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
  const environment = npmEnvironment(join(prefixRoot, "cache"));
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
      const bin =
        typeof metadata.bin === "string" ? metadata.bin : metadata.bin?.zg;
      if (!bin) throw new Error("rollback package has no zg executable");
      const path = join(root, bin);
      const command =
        path.endsWith(".js") || path.endsWith(".cjs") || path.endsWith(".mjs")
          ? process.execPath
          : path;
      const args =
        command === process.execPath ? [path, "--version"] : ["--version"];
      if (run(command, args, { capture: true }).trim() !== baseline)
        throw new Error(
          "rollback executable version differs from the baseline",
        );
    });
  const fresh = join(prefixRoot, "fresh");
  const update = join(prefixRoot, "update");
  await rm(prefixRoot, { recursive: true, force: true });
  install(fresh, release.version);
  await probe({
    manifest,
    entry,
    version: release.version,
    prefix: fresh,
    fixtureRoot: join(prefixRoot, "fresh-workspace"),
  });
  install(update, baseline);
  await verifyPrevious(update);
  install(update, release.version);
  await probe({
    manifest,
    entry,
    version: release.version,
    prefix: update,
    fixtureRoot: join(prefixRoot, "update-workspace"),
  });
  install(update, baseline);
  await verifyPrevious(update);
  console.log(
    `registry install/update/rollback passed: ${baseline} -> ${release.version} -> ${baseline} (${entry.target})`,
  );
}
