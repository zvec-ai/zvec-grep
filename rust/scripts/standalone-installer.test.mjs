import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { chmod, copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { loadReleaseManifest, workspaceDir } from "./npm-package-core.mjs";
import { installerEnvironment } from "./standalone-smoke.mjs";
import { invalidManifestTables } from "./standalone-manifest-fixtures.mjs";
import { parseStandaloneManifest } from "./standalone-manifest.mjs";

const execute = promisify(execFile);
const shellInstaller = join(workspaceDir, "standalone/install.sh");
const powerShellInstaller = join(workspaceDir, "standalone/install.ps1");

async function temporary(t) {
  const root = await mkdtemp(join(tmpdir(), "zg-installer-regression-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  return root;
}

for (const pinned of [true, false]) {
  test(`installers ${pinned ? "resolve pinned versions through v tags" : "download Latest assets from the manifest tag"}`, { timeout: 240000 }, async (t) => {
    const root = await temporary(t);
    const windows = process.platform === "win32";
    const payload = join(root, "zg/bin");
    await mkdir(payload, { recursive: true });
    if (windows) {
      const compiler = join(root, "compile.ps1");
      await writeFile(compiler, `Add-Type -OutputAssembly $env:REVIEW_EXECUTABLE -OutputType ConsoleApplication -TypeDefinition @'
using System;
public class InstallerFixture {
    public static void Main(string[] args) {
        Console.WriteLine(args.Length == 1 && args[0] == "--version" ? "1.2.3" : "native fixture activated");
    }
}
'@\n`);
      await execute("powershell.exe", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", compiler], {
        env: { ...process.env, REVIEW_EXECUTABLE: join(payload, "zg.exe") },
      });
    } else {
      await writeFile(join(payload, "zg"), '#!/bin/sh\nif [ "$1" = --version ]; then printf 1.2.3; else printf "native fixture activated\\n"; fi\n');
      await chmod(join(payload, "zg"), 0o755);
    }
    const extension = windows ? "zip" : "tar.gz";
    const archive = join(root, `payload.${extension}`);
    await execute("tar", [...(windows ? ["-a", "-cf"] : ["-czf"]), archive, "-C", root, "zg"], {
      env: { ...process.env, COPYFILE_DISABLE: "1" },
    });
    const bytes = await readFile(archive);
    const releaseTag = pinned ? "v1.2.3" : "candidate-1.2.3";
    const manifest = { schemaVersion: 1, version: "1.2.3", tag: releaseTag, commit: "a".repeat(40), partial: true,
      assets: loadReleaseManifest().platforms.filter((entry) => entry.os === process.platform).map(({ target }) => ({
        target, accelerator: "cpu", file: `zvec-grep-1.2.3-${target}-cpu.${extension}`,
        bytes: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex"),
      })) };
    const requests = [];
    let corrupt = false;
    const manifestUrl = `${pinned ? "/download/v1.2.3" : "/latest/download"}/manifest.tsv`;
    const manifestTable = () => `schema\t1\nversion\t${manifest.version}\n` + (manifest.tag === undefined ? "" : `tag\t${manifest.tag}\n`) +
      `commit\t${manifest.commit}\npartial\t${manifest.partial}\n` +
      manifest.assets.map((asset) => `${asset.target}\t${asset.accelerator}\t${asset.file}\t${asset.sha256}\t${asset.bytes}\n`).join("");
    let tableOverride;
    const server = createServer((request, response) => {
      requests.push(request.url);
      if (request.url === manifestUrl) {
        response.end(tableOverride ?? manifestTable());
      } else if (manifest.assets.some((asset) => request.url === `/download/${releaseTag}/${asset.file}`)) {
        response.end(corrupt ? Buffer.alloc(bytes.length) : bytes);
      } else response.writeHead(404).end();
    });
    await new Promise((accept, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", accept); });
    const env = { ...process.env, HOME: root, USERPROFILE: root, ZG_INSTALL_TMPDIR: root,
      ZG_INSTALL_REPOSITORY: "zvec-ai/zvec-grep", ZG_INSTALL_ACCELERATOR: "cpu",
      ZG_INSTALL_TEST_RELEASE_URL: `http://127.0.0.1:${server.address().port}`, CI: "true" };
    const args = windows
      ? ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", powerShellInstaller, "-InstallDir", join(root, "安装 with spaces"), "-NoModifyPath", "-SkipAgentUpgrade", ...(pinned ? ["-Version", "1.2.3"] : [])]
      : [shellInstaller, "--install-dir", join(root, "安装 with spaces"), "--skip-agent-upgrade", ...(pinned ? ["--version", "1.2.3"] : [])];
    const install = () => execute(windows ? "powershell.exe" : "sh", args, { env, timeout: 45000 });
    try {
      const result = await install();
      assert.match(result.stdout, /native fixture activated/);
      assert.equal(requests.length, 2);
      assert.equal(requests[0], manifestUrl);
      assert.ok(requests[1].startsWith(`/download/${releaseTag}/zvec-grep-1.2.3-`));
      corrupt = true;
      await assert.rejects(install(), /checksum/i);
      corrupt = false;
      for (const tag of [undefined, "", "../escape", "v1.2.3/other", "v1.2.3?redirect=1", "v1.2.3 other", "v1.2.3\ntag\tother", ...(pinned ? ["other-1.2.3"] : [])]) {
        manifest.tag = tag;
        requests.length = 0;
        await assert.rejects(install(), /release tag/i, `tag=${JSON.stringify(tag)}`);
        assert.deepEqual(requests, [manifestUrl], "invalid tags must fail before requesting an archive");
      }
      manifest.tag = releaseTag;
      if (pinned) {
        const valid = manifestTable();
        for (const table of [valid.replaceAll("\n", "\r\n"), valid.trimEnd(), valid.replace("partial\ttrue", "partial\tfalse")]) {
          tableOverride = table;
          assert.match((await install()).stdout, /native fixture activated/);
        }
        for (const [name, table] of invalidManifestTables(valid)) {
          tableOverride = table;
          requests.length = 0;
          assert.throws(() => parseStandaloneManifest(table), /release manifest/i, name);
          await assert.rejects(install(), /release manifest/i, name);
          assert.deepEqual(requests, [manifestUrl], `${name} must fail before requesting an archive`);
        }
      }
    } finally {
      server.closeAllConnections();
      await new Promise((accept) => server.close(accept));
    }
  });
}

test("the Node-free smoke environment can extract gzip archives", { skip: process.platform === "win32" }, async (t) => {
  const root = await temporary(t);
  const env = await installerEnvironment(root);
  await writeFile(join(root, "payload"), "native payload");
  const archive = join(root, "payload.tar.gz");
  await execute("tar", ["-czf", archive, "-C", root, "payload"]);
  const output = join(root, "extracted");
  await mkdir(output);
  await execute("tar", ["-xzf", archive, "-C", output], { env });
  assert.equal(await readFile(join(output, "payload"), "utf8"), "native payload");
  await assert.rejects(execute("node", ["--version"], { env }), { code: "ENOENT" });
});

test("shell uninstall and offline activation use root state when current is missing", { skip: process.platform === "win32" }, async (t) => {
  const root = await temporary(t);
  const install = join(root, "安装 with spaces/dist");
  const executable = join(install, "1.2.3/darwin-arm64-cpu/bin/zg");
  await mkdir(dirname(executable), { recursive: true });
  await writeFile(executable, '#!/bin/sh\nif [ "$1" = --version ]; then printf 1.2.3; else printf "%s\\n" "$@" > "$REVIEW_ARGUMENTS"; fi\n');
  await chmod(executable, 0o755);
  await writeFile(join(install, "install-state.tsv"), "bundle\t1.2.3/darwin-arm64-cpu\nversion\t1.2.3\nrepository\tegolearner/zvec-grep\n");
  await writeFile(join(install, "activation.json"), "{}");
  const output = join(root, "arguments");
  const env = { ...process.env, HOME: root, ZG_INSTALL_DIR: install, ZG_INSTALL_TMPDIR: root,
    ZG_INSTALL_REPOSITORY: "", ZG_INSTALL_ACCELERATOR: "cpu", REVIEW_ARGUMENTS: output };
  await execute("sh", [shellInstaller, "--uninstall"], { env });
  const args = (await readFile(output, "utf8")).split("\n");
  assert.ok(args.includes("--remove"));
  assert.equal(args[args.indexOf("--repository") + 1], "egolearner/zvec-grep");
  await execute("sh", [shellInstaller, "--activate", "1.2.2"], { env });
  const activation = (await readFile(output, "utf8")).split("\n");
  assert.equal(activation[activation.indexOf("--activate") + 1], "1.2.2");
  await assert.rejects(execute("sh", [shellInstaller, "--activate", "1.2.2", "--uninstall"], { env }), /cannot be combined/);
  for (const repository of ["https://example.com/repo", "owner/../repo", "../repo", "owner/repo?redirect=1"]) {
    await assert.rejects(execute("sh", [shellInstaller, "--uninstall"], { env: { ...env, ZG_INSTALL_REPOSITORY: repository } }), /GitHub owner\/name/);
  }
  const probes = join(root, "platform-probes");
  await mkdir(probes);
  for (const [name, source] of Object.entries({
    uname: '#!/bin/sh\ncase "$1" in -s) printf Linux;; -m) printf x86_64;; esac\n',
    getconf: '#!/bin/sh\nprintf "glibc %s\\n" "$REVIEW_GLIBC"\n',
  })) {
    await writeFile(join(probes, name), source);
    await chmod(join(probes, name), 0o755);
  }
  const linux = { ...env, PATH: `${probes}:${env.PATH}`, REVIEW_GLIBC: "2.28" };
  await execute("sh", [shellInstaller, "--uninstall"], { env: linux });
  await assert.rejects(execute("sh", [shellInstaller, "--uninstall"], { env: { ...linux, REVIEW_GLIBC: "2.27" } }), /glibc 2\.28 or newer/);
});

test("GPU detection terminates a probe that ignores SIGTERM", { skip: process.platform === "win32", timeout: 12000 }, async (t) => {
  const root = await temporary(t);
  const source = await readFile(shellInstaller, "utf8");
  const probe = source.match(/^has_nvidia\(\) \{[\s\S]*?^\}/m)?.[0];
  assert.ok(probe);
  const fake = join(root, "nvidia-smi");
  await writeFile(fake, '#!/bin/sh\ntrap "" TERM\nexec sleep 30\n');
  await chmod(fake, 0o755);
  const child = spawn("sh", ["-c", `${probe}\nif has_nvidia; then exit 2; fi\nprintf completed`], {
    detached: true, env: { ...process.env, PATH: `${root}:${process.env.PATH}`, staging: root }, stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "";
  child.stdout.on("data", (chunk) => { stdout += chunk; });
  child.stderr.resume();
  let timer;
  try {
    const code = await Promise.race([
      new Promise((accept, reject) => { child.once("error", reject); child.once("close", accept); }),
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error("GPU probe exceeded its hard deadline")), 8000); }),
    ]);
    assert.equal(code, 0);
    assert.equal(stdout, "completed");
  } finally {
    clearTimeout(timer);
    try { process.kill(-child.pid, "SIGKILL"); } catch (error) { if (error.code !== "ESRCH") throw error; }
  }
});

test("Windows PATH updates respect opt-out and command ownership", { skip: process.platform !== "win32" }, async (t) => {
  const root = await temporary(t);
  const source = await readFile(powerShellInstaller, "utf8");
  const update = source.match(/^function Get-UpdatedUserPath\([^\n]+\) \{[\s\S]*?^\}/m)?.[0];
  assert.ok(update);
  const script = join(root, "path-test.ps1");
  await writeFile(script, `${update}\n$ErrorActionPreference = 'Stop'
$pathDir = 'C:\\Program Files\\zg\\bin'
$original = "C:\\user;$pathDir;C:\\tools"
foreach ($remove in @($true, $false)) {
    if ($null -ne (Get-UpdatedUserPath $original $pathDir $remove $false $true)) { throw 'Opt-out must leave PATH unchanged' }
}
if ($null -ne (Get-UpdatedUserPath $original $pathDir $true $true $false)) { throw 'Uninstall must preserve a user-owned entry' }
if ((Get-UpdatedUserPath $original $pathDir $true $true $true) -ne 'C:\\user;C:\\tools') { throw 'Remove only the managed entry' }
$installed = Get-UpdatedUserPath 'C:\\user' $pathDir $false $true $false
if ((Get-UpdatedUserPath $installed $pathDir $false $true $true) -ne $installed) { throw 'Install must be idempotent' }
`);
  await execute("powershell.exe", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", script]);
});

test("shell staged version checks reject mismatches and have a hard deadline", { skip: process.platform === "win32", timeout: 16000 }, async (t) => {
  const root = await temporary(t);
  const source = await readFile(shellInstaller, "utf8");
  const check = source.match(/^check_version\(\) \{[\s\S]*?^\}/m)?.[0];
  assert.ok(check);
  const fake = join(root, "zg");
  const env = { ...process.env, staging: root, REVIEW_EXECUTABLE: fake };
  await writeFile(fake, '#!/bin/sh\nprintf 1.2.3\n');
  await chmod(fake, 0o755);
  await execute("sh", ["-c", `${check}\ncheck_version "$REVIEW_EXECUTABLE" 1.2.3`], { env });
  await assert.rejects(execute("sh", ["-c", `${check}\ncheck_version "$REVIEW_EXECUTABLE" 1.2.4`], { env }));
  await writeFile(fake, '#!/bin/sh\ntrap "" TERM\nexec sleep 30\n');
  await assert.rejects(execute("sh", ["-c", `${check}\ncheck_version "$REVIEW_EXECUTABLE" 1.2.3`], {
    env, timeout: 13000, killSignal: "SIGKILL",
  }), (error) => {
    assert.equal(error.killed, false, "the installer must terminate the probe before the test deadline");
    assert.equal(error.code, 1);
    return true;
  });
});

test("PowerShell staged version checks accept only the expected executable version", { skip: process.platform !== "win32" }, async (t) => {
  const root = await temporary(t);
  const source = await readFile(powerShellInstaller, "utf8");
  const check = source.match(/^function Test-StagedVersion\([^\n]+\) \{[\s\S]*?^\}/m)?.[0];
  assert.ok(check);
  const script = join(root, "version-test.ps1");
  await writeFile(script, `${check}\n$ErrorActionPreference = 'Stop'\nTest-StagedVersion $env:REVIEW_EXECUTABLE $env:REVIEW_VERSION\n`);
  const env = { ...process.env, REVIEW_EXECUTABLE: process.execPath, REVIEW_VERSION: process.version };
  await execute("powershell.exe", ["-NoProfile", "-File", script], { env });
  await assert.rejects(execute("powershell.exe", ["-NoProfile", "-File", script], {
    env: { ...env, REVIEW_VERSION: "wrong" },
  }), /version mismatch/);
});

test("PowerShell uninstall reaches the staged executable when current is missing", { skip: process.platform !== "win32" }, async (t) => {
  const root = await temporary(t);
  const install = join(root, "program");
  await mkdir(join(install, "1.2.3/win32-x64-msvc-cpu/bin"), { recursive: true });
  // Node rejects the activation flag, proving that the retained bundle was staged and invoked.
  await copyFile(process.execPath, join(install, "1.2.3/win32-x64-msvc-cpu/bin/zg.exe"));
  await writeFile(join(install, "install-state.tsv"), `bundle\t1.2.3/win32-x64-msvc-cpu\nversion\t${process.version}\n`);
  await writeFile(join(install, "activation.json"), "{}");
  for (const accelerator of [[], ["-Accelerator", "cpu"]]) {
    await assert.rejects(execute("powershell.exe", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", powerShellInstaller,
      "-InstallDir", install, ...accelerator, "-NoModifyPath", "-Uninstall"], {
      env: { ...process.env, ZG_INSTALL_TMPDIR: root, ZG_INSTALL_REPOSITORY: "zvec-ai/zvec-grep", ZG_INSTALL_ACCELERATOR: "" },
    }), (error) => {
      assert.match(error.stderr, /bad option: --standalone/);
      assert.doesNotMatch(error.stderr, /No standalone installation found/);
      return true;
    });
  }
});

const action = (await readFile(new URL("../../.github/actions/cuda-cli-e2e/action.yml", import.meta.url), "utf8"))
  .replaceAll("\r\n", "\n");

for (const platform of ["Linux", "Windows"]) {
  test(`${platform} CUDA 12 and 13 consume the standalone smoke bundle result`, {
    skip: (process.platform === "win32") !== (platform === "Windows"),
  }, async (t) => {
    const root = await mkdtemp(join(tmpdir(), "zg-cuda-contract-"));
    t.after(() => rm(root, { recursive: true, force: true }));
    const bundle = join(root, "安装 with spaces", "9.8.7", "actual-cuda-bundle");
    await mkdir(join(bundle, "lib"), { recursive: true });
    await mkdir(join(root, "rust", "scripts"), { recursive: true });
    // Use a different layout so the consumer must trust the producer's result.
    const smoke = `console.log(JSON.stringify({bundle: process.env.REVIEW_BUNDLE, version: "9.8.7"}));\n`;
    await writeFile(join(root, "rust", "scripts", "standalone-smoke.mjs"), smoke);
    await mkdir(join(root, "scripts"));
    await writeFile(join(root, "scripts", "standalone-smoke.mjs"), smoke);
    const probe = join(root, "gpu-probe.mjs");
    const calls = join(root, "gpu-calls.jsonl");
    await writeFile(probe, `import assert from "node:assert/strict";
import { appendFileSync, statSync } from "node:fs";
import { join } from "node:path";
const args = process.argv.slice(2);
const bundle = args[args.indexOf("--bundle") + 1];
assert.equal(bundle, process.env.REVIEW_BUNDLE);
assert.ok(statSync(join(bundle, "lib")).isDirectory());
appendFileSync(process.env.REVIEW_CALLS, JSON.stringify(args) + "\\n");
`);
    const env = { ...process.env, RUNNER_TEMP: root, REVIEW_BUNDLE: bundle, REVIEW_CALLS: calls, REVIEW_GPU_PROBE: probe };
    for (const name of [
      `Rebuild the ${platform} CPU installation for GGUF and ONNX CUDA 12`,
      platform === "Windows" ? "Run Windows GGUF primary with ONNX still on CUDA 12" : "Run Linux GGUF primary on CUDA 13.1",
    ]) {
      const step = action.split(`    - name: ${name}\n`)[1]?.split("\n    - name:")[0];
      assert.ok(step, `missing action step: ${name}`);
      const block = platform === "Windows"
        ? step.match(/^        if \('\$\{\{ inputs\.standalone_directory \}\}'\) \{([\s\S]*?)^        \}/m)?.[1]
        : step.match(/^            if \[ -n "\$\{\{ inputs\.standalone_directory \}\}" \]; then([\s\S]*?)^            fi/m)?.[1];
      assert.ok(block, `missing standalone invocation: ${name}`);
      const script = block.replaceAll("${{ inputs.standalone_directory }}", "archives").replaceAll("/tmp/zg-install", root);
      if (platform === "Windows") {
        const path = join(root, "step.ps1");
        await writeFile(path, `$ErrorActionPreference = 'Stop'\nfunction python { & node $env:REVIEW_GPU_PROBE @args }\n${script}`);
        await execute("powershell.exe", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", path], { cwd: root, env });
      } else {
        await execute("bash", ["-euo", "pipefail", "-c", `python3() { node "$REVIEW_GPU_PROBE" "$@"; }\n${script}`], { cwd: root, env });
      }
    }
    const recorded = (await readFile(calls, "utf8")).trim().split("\n").map((line) => JSON.parse(line));
    assert.deepEqual(recorded.map((args) => args[args.indexOf("--expected-gguf-cuda") + 1]), ["12.4", "13.1"]);
  });
}
