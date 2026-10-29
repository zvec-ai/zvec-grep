import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { installedCliEnvironment, installedCliInvocation, runInstalledCli } from "./npm-installed-cli.mjs";
import { npmEnvironment, run } from "./npm-package-core.mjs";
import { smokeInstalledModels, smokeModels } from "./npm-model-smoke.mjs";

async function temporary(t) {
  const root = await mkdtemp(join(tmpdir(), "zg-npm-cli-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  return root;
}

test("executes npm's real global entry, preserving spaced arguments and retaining native failures", async (t) => {
  const root = await temporary(t);
  const source = join(root, "fixture");
  const prefix = join(root, "prefix with spaces");
  await mkdir(source);
  await writeFile(join(source, "package.json"), JSON.stringify({
    name: "zg-entry-fixture", version: "1.0.0", bin: { zg: "cli.cjs" },
  }));
  await writeFile(join(source, "cli.cjs"), `#!/usr/bin/env node
console.log(JSON.stringify({args: process.argv.slice(2), home: process.env.ZVEC_GREP_HOME}));
if (process.argv.includes("--fail")) { console.error("fixture native failure"); process.exit(7); }
`, { mode: 0o755 });
  run("npm", ["install", "--global", "--prefix", prefix, "--offline", "--ignore-scripts", "--no-audit", "--no-fund", source],
    { env: npmEnvironment(join(root, "npm-cache")), capture: true });
  const env = installedCliEnvironment(prefix, join(root, "runtime"));
  const result = runInstalledCli(prefix, ["two words", "中文路径"], { env });
  assert.deepEqual(JSON.parse(result.stdout), { args: ["two words", "中文路径"], home: join(root, "runtime") });
  const logRoot = join(root, "logs");
  assert.throws(() => runInstalledCli(prefix, ["--fail"], { env, logRoot, label: "failure" }), /fixture native failure/);
  assert.match(await readFile(join(logRoot, "failure.stderr"), "utf8"), /fixture native failure/);
  await rm(installedCliInvocation(prefix, []).entry);
  assert.throws(() => runInstalledCli(prefix, ["--version"], { env }), /ENOENT/);
});

test("Windows executes the npm PowerShell shim, not the package executable", () => {
  const invocation = installedCliInvocation("prefix", ["--vector", "two words"], "win32");
  assert.equal(invocation.command, "pwsh");
  assert.ok(invocation.entry.endsWith("zg.ps1"));
  assert.deepEqual(invocation.args.slice(-3), [invocation.entry, "--vector", "two words"]);
});

function successfulCli(calls) {
  return (prefix, args, options) => {
    calls.push({ prefix, args, ...options });
    return {
      stdout: `releaseobservatorymarker\n${smokeModels.map(([, model]) => model).join("\n")}\nServer: ready\nServer: stopped\n${args.includes("releaseincrementalmarker") && options.label === "update-query" ? "update.md" : ""}`,
      stderr: "Index complete",
    };
  };
}

test("all three models must index, query through both transports, and complete MCP", async (t) => {
  const root = await temporary(t);
  const calls = [];
  const mcp = [];
  const results = await smokeInstalledModels({ prefix: join(root, "prefix"), fixtureRoot: root,
    execute: successfulCli(calls), mcpProbe: async (options) => mcp.push(options) });
  assert.deepEqual(results.map(({ model, passed }) => [model, passed]), smokeModels.map(([, model]) => [model, true]));
  for (const [, model] of smokeModels) assert.ok(calls.some(({ args }) => args.includes("--embedding") && args.includes(model)));
  assert.equal(calls.filter(({ label }) => label === "vector").length, 3);
  assert.equal(calls.filter(({ label, env }) => label === "server-vector" && env.ZVEC_GREP_MODE === "server").length, 3);
  assert.equal(mcp.length, 3);
  assert.equal(calls.filter(({ label }) => label === "server-off").length, 3);
});

test("a successful exit with wrong vector results fails the model gate", async (t) => {
  const root = await temporary(t);
  const execute = successfulCli([]);
  await assert.rejects(smokeInstalledModels({ prefix: root, fixtureRoot: root,
    execute: (...args) => args[2].label === "vector" ? { stdout: "unrelated result", stderr: "" } : execute(...args),
    mcpProbe: async () => assert.fail("must reject the wrong result first"),
  }), /npm entry model smoke failed/);
  const results = JSON.parse(await readFile(join(root, "results.json"), "utf8"));
  assert.equal(results.filter(({ passed }) => !passed).length, 3);
});

test("MCP failures fail the gate and still stop each daemon", async (t) => {
  const root = await temporary(t);
  const calls = [];
  await assert.rejects(smokeInstalledModels({ prefix: root, fixtureRoot: root,
    execute: successfulCli(calls), mcpProbe: async () => { throw new Error("MCP unavailable"); },
  }), /npm entry model smoke failed/);
  assert.equal(calls.filter(({ label }) => label === "server-off").length, 3);
  const results = JSON.parse(await readFile(join(root, "results.json"), "utf8"));
  assert.ok(results.every(({ passed, error }) => !passed && error === "MCP unavailable"));
});

test("latest requires successful completion of the full six-platform registry matrix", async () => {
  const workflow = await readFile(new URL("../../.github/workflows/rust-publish.yml", import.meta.url), "utf8");
  const registry = workflow.split("\n  registry-smoke:\n")[1].split("\n  promote:\n")[0];
  const manifest = JSON.parse(await readFile(new URL("../npm/platforms.json", import.meta.url), "utf8"));
  assert.deepEqual([...registry.matchAll(/- target: ([\w-]+)/g)].map((match) => match[1]).sort(),
    manifest.platforms.map(({ target }) => target).sort());
  assert.match(registry, /fail-fast: false/);
  assert.match(registry, /npm run release:registry-smoke/);
  assert.match(registry, /if: \$\{\{ always\(\) \}\}/);
  assert.doesNotMatch(registry, /continue-on-error/);
  const promote = workflow.split("\n  promote:\n")[1];
  assert.match(promote, /needs: \[source, publish, registry-smoke\]/);
  assert.match(promote, /needs\.registry-smoke\.result == 'success'/);
});
