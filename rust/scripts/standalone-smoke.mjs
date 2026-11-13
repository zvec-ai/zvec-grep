#!/usr/bin/env node

import assert from "node:assert/strict";
import { createReadStream, existsSync } from "node:fs";
import { lstat, mkdir, readFile, readlink, realpath, symlink, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { createServer as createTcpServer } from "node:net";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { delimiter, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { loadReleaseManifest, selectPlatform, workspaceDir } from "./npm-package-core.mjs";
import { parseStandaloneManifest } from "./standalone-manifest.mjs";

const execute = promisify(execFile);
const scriptPath = fileURLToPath(import.meta.url);

async function freePort() {
  const socket = createTcpServer();
  await new Promise((accept, reject) => { socket.once("error", reject); socket.listen(0, "127.0.0.1", accept); });
  const port = socket.address().port;
  await new Promise((accept) => socket.close(accept));
  return port;
}

export async function installerEnvironment(root) {
  const home = join(root, process.platform === "win32" ? "home" : "安装 with spaces");
  const temporary = join(root, "temporary");
  await mkdir(home, { recursive: true });
  await mkdir(temporary, { recursive: true });
  const env = { ...process.env, HOME: home, USERPROFILE: home, APPDATA: join(home, "AppData/Roaming"),
    XDG_CONFIG_HOME: join(home, ".config"), ZVEC_GREP_HOME: join(home, ".zvec-grep"),
    ZG_INSTALL_TMPDIR: temporary, ZG_INSTALL_SKIP_AGENT_UPGRADE: "1", CI: "true" };
  for (const key of ["ZG_INSTALL_DIR", "ZG_INSTALL_ACCELERATOR", "ZVEC_GREP_MCP_TOOLSET"]) delete env[key];
  if (process.platform === "win32") {
    env.PATH = process.env.PATH.split(delimiter).filter((path) => !existsSync(join(path, "node.exe")) && !existsSync(join(path, "npm.cmd"))).join(delimiter);
  } else {
    const bin = join(root, "system-tools");
    await mkdir(bin, { recursive: true });
    for (const command of ["sh", "curl", "wget", "tar", "gzip", "awk", "grep", "sed", "uname", "sw_vers", "sysctl", "getconf", "cut", "id", "mkdir", "mktemp", "rm", "cp", "cat", "sleep", "wc", "tr", "sha256sum", "shasum", "nvidia-smi"]) {
      const source = process.env.PATH.split(delimiter).map((path) => join(path, command)).find(existsSync);
      if (source && !existsSync(join(bin, command))) await symlink(source, join(bin, command));
    }
    env.PATH = bin;
  }
  return env;
}

export async function smokeStandalone({ directory, prefixRoot, expectCuda = false, keepInstall = false }) {
  directory = resolve(directory);
  prefixRoot = resolve(prefixRoot);
  const inventory = parseStandaloneManifest(await readFile(join(directory, "manifest.tsv"), "utf8"));
  const entry = selectPlatform(loadReleaseManifest(), process.platform, process.arch, process.report?.getReport().header ?? {});
  const env = await installerEnvironment(prefixRoot);
  const installDir = process.platform === "win32" ? join(prefixRoot, "安装 with spaces") : join(env.HOME, ".zvec-grep/dist");
  const profiles = [".profile", ".bashrc", ".bash_profile", ".zshrc", ".config/fish/config.fish"];
  const profileContent = "# Existing user shell configuration\n";
  for (const profile of profiles) {
    const path = join(env.HOME, profile);
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, profileContent);
  }
  const requests = [];
  let corrupt = false;
  const allowed = new Set(["manifest.tsv", ...inventory.assets.filter((asset) => asset.target === entry.target).map((asset) => asset.file)]);
  const server = createServer((request, response) => {
    requests.push(request.url);
    const match = request.url.match(/^\/(?:latest\/download|download\/[^/]+)\/([^/]+)$/);
    const file = match?.[1];
    if (!allowed.has(file) || (request.url.startsWith("/download/") && !request.url.startsWith(`/download/${inventory.tag}/`))) {
      response.writeHead(404).end(); return;
    }
    if (corrupt && /\.(zip|gz)$/.test(file)) { response.end("corrupt archive"); return; }
    const stream = createReadStream(join(directory, file));
    stream.on("error", () => response.destroy());
    stream.pipe(response);
  });
  await new Promise((accept, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", accept); });
  const baseUrl = `http://127.0.0.1:${server.address().port}`;
  env.ZG_INSTALL_TEST_RELEASE_URL = baseUrl;
  const nativeBinary = join(installDir, inventory.version, `${entry.target}-cpu`, "bin", entry.binary);
  const binary = process.platform === "win32" ? join(installDir, "bin", entry.binary) : join(env.HOME, ".local/bin/zg");
  const log = [];
  const invoke = async (command, args, environment = env) => {
    try {
      const result = await execute(command, args, { env: environment, timeout: 180_000, maxBuffer: 8 * 1024 * 1024, windowsHide: true });
      log.push({ command, args, stdout: result.stdout, stderr: result.stderr });
      return result.stdout;
    } catch (error) {
      log.push({ command, args, code: error.code, signal: error.signal, stdout: error.stdout, stderr: error.stderr });
      throw error;
    }
  };
  const install = async ({ accelerator, uninstall = false, version, activate, onPath = false, shell } = {}) => {
    const args = process.platform === "win32"
      ? ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", join(directory, "install.ps1"), "-InstallDir", installDir, "-NoModifyPath", "-SkipAgentUpgrade"]
      : [join(directory, "install.sh"), "--skip-agent-upgrade"];
    const option = (unix, windows, value) => { args.push(process.platform === "win32" ? windows : unix); if (value !== undefined) args.push(value); };
    if (activate) option("--activate", "-ActivateVersion", activate);
    if (version) option("--version", "-Version", version);
    if (accelerator) option("--accelerator", "-Accelerator", accelerator);
    if (uninstall) option("--uninstall", "-Uninstall");
    const environment = { ...env, ...(onPath ? { PATH: `${dirname(binary)}${delimiter}${env.PATH}` } : {}), ...(shell ? { SHELL: shell } : {}) };
    return invoke(process.platform === "win32" ? "powershell.exe" : "sh", args, environment);
  };
  let running = false;
  try {
    const firstOutput = await install(expectCuda ? { accelerator: "cpu" } : {});
    if (process.platform !== "win32") {
      assert.equal(await readlink(binary), join(await realpath(installDir), "current/bin/zg"));
      assert.equal(await realpath(binary), await realpath(nativeBinary));
      assert.match(firstOutput, /not in your current PATH/);
    }
    assert.equal((await invoke(binary, ["--version"])).trim(), inventory.version);
    let receipt = JSON.parse(await readFile(join(installDir, "install-receipt.json"), "utf8"));
    assert.equal(receipt.acceleratorActual, "cpu");
    assert.equal(requests.some((path) => /-cuda\.(zip|tar.gz)$/.test(path)), false, "CPU installation must not download CUDA archives");
    const data = join(env.ZVEC_GREP_HOME, "user-data");
    await writeFile(data, "preserve configuration and indexes");
    const port = await freePort();
    await invoke(binary, ["--server", "on", "--home", env.ZVEC_GREP_HOME, "--listen", `127.0.0.1:${port}`]);
    running = true;
    const instance = async () => JSON.parse(await readFile(join(env.ZVEC_GREP_HOME, "daemon/instance.lock"), "utf8"));
    const before = await instance();
    const upgradeOutput = await install({ ...(expectCuda ? { accelerator: "auto" } : {}), onPath: true });
    assert.doesNotMatch(upgradeOutput, /not in your current PATH/);
    assert.notEqual((await instance()).instanceToken, before.instanceToken, "upgrade must replace the old daemon");
    receipt = JSON.parse(await readFile(join(installDir, "install-receipt.json"), "utf8"));
    assert.equal(receipt.acceleratorActual, expectCuda ? "cuda" : "cpu");
    assert.equal(receipt.source, "https://github.com/zvec-ai/zvec-grep/releases");
    await invoke(binary, ["--server", "status", "--check-ready", "--home", env.ZVEC_GREP_HOME]);
    const corpus = join(prefixRoot, "corpus.txt");
    await writeFile(corpus, "standalone installation probe\n");
    const direct = await invoke(binary, ["--rg", "--mode", "direct", "-F", "standalone installation probe", corpus]);
    const automatic = await invoke(binary, ["--rg", "-F", "standalone installation probe", corpus]);
    assert.equal(direct, automatic);
    const requestCount = requests.length;
    const beforeRollback = await instance();
    await install({ activate: inventory.version, accelerator: "cpu" });
    assert.equal(requests.length, requestCount, "retained version activation must be offline");
    assert.notEqual((await instance()).instanceToken, beforeRollback.instanceToken);
    receipt = JSON.parse(await readFile(join(installDir, "install-receipt.json"), "utf8"));
    assert.equal(receipt.acceleratorActual, "cpu");
    if (expectCuda) {
      assert.ok(existsSync(join(installDir, inventory.version, `${entry.target}-cuda`)));
      await install({ activate: inventory.version, accelerator: "cuda" });
      assert.equal(requests.length, requestCount);
    }
    const active = await instance();
    corrupt = true;
    await assert.rejects(install(), /Command failed/);
    corrupt = false;
    assert.equal((await instance()).instanceToken, active.instanceToken, "download failure must leave the running version intact");
    assert.equal(await readFile(data, "utf8"), "preserve configuration and indexes");
    const pinnedOutput = await install({ version: inventory.version, shell: "/usr/bin/fish" });
    if (process.platform !== "win32") assert.match(pinnedOutput, /fish_add_path "\$HOME\/\.local\/bin"/);
    if (!expectCuda) assert.equal(requests.some((path) => /-cuda\.(zip|tar.gz)$/.test(path)), false);
    await invoke(binary, ["--server", "off", "--home", env.ZVEC_GREP_HOME]);
    running = false;
    if (!keepInstall) {
      await install({ uninstall: true });
      assert.equal(existsSync(binary), false);
      assert.equal(await lstat(binary).catch(() => undefined), undefined, "uninstall must remove the owned link, not leave it dangling");
      assert.equal(await readFile(data, "utf8"), "preserve configuration and indexes");
    }
    for (const profile of profiles) assert.equal(await readFile(join(env.HOME, profile), "utf8"), profileContent);
    await writeFile(join(prefixRoot, "results.json"), `${JSON.stringify({ passed: true, target: entry.target, version: inventory.version, expectCuda, requests, log }, null, 2)}\n`);
    return { bundle: join(installDir, inventory.version, `${entry.target}-${expectCuda ? "cuda" : "cpu"}`), version: inventory.version };
  } finally {
    if (running && existsSync(binary)) await invoke(binary, ["--server", "off", "--home", env.ZVEC_GREP_HOME]).catch(() => {});
    server.closeAllConnections();
    await new Promise((accept) => server.close(accept));
    await writeFile(join(prefixRoot, "commands.json"), `${JSON.stringify({ requests, log }, null, 2)}\n`);
  }
}

if (process.argv[1] && resolve(process.argv[1]) === scriptPath) {
  const args = process.argv.slice(2);
  const options = { directory: join(workspaceDir, "dist/standalone"), prefixRoot: join(workspaceDir, "dist/standalone-smoke") };
  for (let index = 0; index < args.length; index++) {
    const argument = args[index];
    if (argument === "--expect-cuda") options.expectCuda = true;
    else if (argument === "--keep-install") options.keepInstall = true;
    else if (["--directory", "--prefix-root"].includes(argument) && args[index + 1]) options[argument === "--directory" ? "directory" : "prefixRoot"] = args[++index];
    else throw new Error(`unknown option: ${argument}`);
  }
  smokeStandalone(options).then((result) => console.log(JSON.stringify(result))).catch((error) => { console.error(error); process.exitCode = 1; });
}
