import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { closeInstalledCli, installedCliEnvironment, runInstalledCli, spawnInstalledCli } from "./npm-installed-cli.mjs";

export const smokeModels = [
  ["potion", "local/potion-code-16m-v2"],
  ["gguf", "local/embeddinggemma-300m"],
  ["onnx", "local/all-minilm-l6-v2"],
];
const marker = "releaseobservatorymarker";

function requireOutput(output, expected) {
  if (!output.includes(expected)) throw new Error(`npm zg output is missing ${expected}:\n${output.slice(-4000)}`);
}

function requireIndexProgress(output) {
  // Progress is not a readiness verdict: keep the --check-ready and real query
  // checks below after validating the non-TTY finalization stage sequence.
  const lines = output.split(/\r?\n/);
  let cursor = 0;
  for (const stage of ["Optimizing index", "Saving index", "Finishing index"]) {
    while (cursor < lines.length && lines[cursor] !== stage && !lines[cursor].startsWith(`${stage}: `)) cursor++;
    if (cursor === lines.length) throw new Error(`npm zg output has a missing or out-of-order index progress stage: ${stage}\n${output.slice(-4000)}`);
    cursor++;
  }
}

async function availablePort() {
  const server = createServer();
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = server.address().port;
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  return port;
}

export async function smokeInstalledMcp({ prefix, cwd, env, logRoot }) {
  const child = spawnInstalledCli(prefix, ["--server", "--stdio"], { cwd, env, stdio: "pipe" });
  const lines = createInterface({ input: child.stdout });
  const pending = new Map();
  const transcript = [];
  let stderr = "";
  const fail = (error) => { for (const waiter of pending.values()) waiter.reject(error); pending.clear(); };
  child.on("error", fail);
  child.stdin.on("error", fail);
  child.on("exit", (code) => fail(new Error(`npm MCP entry exited before responding: ${code}`)));
  child.stderr.on("data", (data) => { stderr += data; });
  lines.on("line", (line) => {
    try {
      const response = JSON.parse(line);
      transcript.push(response);
      const waiter = pending.get(response.id);
      if (waiter) {
        pending.delete(response.id);
        if (response.error) waiter.reject(new Error(JSON.stringify(response.error)));
        else waiter.resolve(response.result);
      }
    } catch (error) { fail(error); }
  });
  const request = (id, method, params) => new Promise((resolve, reject) => {
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`MCP ${method} timed out`)); }, 60_000);
    pending.set(id, {
      resolve: (result) => { clearTimeout(timer); resolve(result); },
      reject: (error) => { clearTimeout(timer); reject(error); },
    });
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
  });
  try {
    await request(1, "initialize", { protocolVersion: "2024-11-05", capabilities: {},
      clientInfo: { name: "npm-release-smoke", version: "1" } });
    child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);
    const tools = await request(2, "tools/list", {});
    if (!tools.tools.some((tool) => tool.name === "zvec_grep_search")) throw new Error("MCP search tool missing");
    const result = await request(3, "tools/call", { name: "zvec_grep_search",
      arguments: { root: cwd, fts: [marker], preview: "full" } });
    if (result.isError) throw new Error(JSON.stringify(result));
    requireOutput(JSON.stringify(result), marker);
  } finally {
    try {
      // Wait for EOF shutdown; a stuck shim/native child requires tree cleanup.
      await closeInstalledCli(child);
    } finally {
      lines.close();
      await mkdir(logRoot, { recursive: true });
      await writeFile(join(logRoot, "mcp.json"), `${JSON.stringify(transcript, null, 2)}\n`);
      await writeFile(join(logRoot, "mcp.stderr"), stderr);
    }
  }
}

export async function smokeInstalledModels({ prefix, fixtureRoot, modelCache,
  execute = runInstalledCli, mcpProbe = smokeInstalledMcp }) {
  modelCache ??= process.env.ZG_RELEASE_MODEL_CACHE ?? join(fixtureRoot, "model-cache");
  const results = [];
  await mkdir(fixtureRoot, { recursive: true });
  for (const [backend, model] of smokeModels) {
    const root = join(fixtureRoot, backend);
    // zvec nests index files below the workspace. Keep Windows below MAX_PATH,
    // while retaining logs under the artifact directory and testing spaced paths.
    const temporary = process.platform === "win32"
      ? await mkdtemp(join(process.env.RUNNER_TEMP ?? tmpdir(), "zg-")) : undefined;
    const cwd = join(temporary ?? root, "workspace 中文 space");
    const logRoot = join(root, "logs");
    await rm(cwd, { recursive: true, force: true });
    await mkdir(cwd, { recursive: true });
    await writeFile(join(cwd, "observatory.md"),
      `# Observatory\nThe heliotrope capacitor calibrates the observatory telescope.\n${marker}\n`);
    await writeFile(join(cwd, "cooking.md"), "# Recipe\nMix flour with water to bake bread.\n");
    const env = installedCliEnvironment(prefix, join(root, "home"), {
      ZVEC_GREP_MODEL_CACHE: modelCache,
      RUST_LOG: "warn,zg_engine=debug,ort=info,llama-cpp-2=info",
    });
    const run = (label, args, mode = "direct") => execute(prefix, args, {
      cwd, env: { ...env, ZVEC_GREP_MODE: mode }, logRoot, label, timeout: 900_000,
    });
    const runtime = ["--device", "cpu", "--model-cache", modelCache, "--debug", "--no-color"];
    let started = false;
    try {
      const index = await run("index", ["--index", "--embedding", model, ...runtime]);
      requireIndexProgress(index.stderr);
      requireOutput((await run("status", ["--status", "--check-ready", "--no-color"])).stdout, model);
      const vector = ["--vector", "Which component calibrates the telescope?", "--refresh", "off",
        "--limit", "1", "--preview", "full", ...runtime];
      requireOutput((await run("vector", vector)).stdout, marker);
      requireOutput((await run("fts", ["--fts", marker, "--refresh", "off", "--preview", "full"])).stdout, marker);
      requireOutput((await run("hybrid", ["Which component calibrates the telescope?", "--refresh", "off",
        "--limit", "1", "--preview", "full", ...runtime])).stdout, marker);

      await writeFile(join(cwd, "update.md"), "releaseincrementalmarker calibrates the telescope.\n");
      await run("update-index", ["--index", ...runtime]);
      requireOutput((await run("update-query", ["--fts", "releaseincrementalmarker", "--refresh", "off", "--preview", "full"])).stdout,
        "update.md");
      await rm(join(cwd, "update.md"));
      await run("delete-index", ["--index", ...runtime]);
      const removed = (await run("delete-query", ["--fts", "releaseincrementalmarker", "--refresh", "off"])).stdout;
      if (removed.includes("update.md")) throw new Error("deleted file remains searchable");

      const port = await availablePort();
      env.ZVEC_GREP_SERVER_URL = `http://127.0.0.1:${port}/mcp`;
      started = true; // Cleanup also covers a partially failed startup.
      await run("server-on", ["--server", "on", "--listen", `127.0.0.1:${port}`]);
      requireOutput((await run("server-ready", ["--server", "status", "--check-ready"])).stdout, "Server: ready");
      requireOutput((await run("server-vector", vector, "server")).stdout, marker);
      await mcpProbe({ prefix, cwd, env, logRoot });
      results.push({ backend, model, passed: true });
    } catch (error) {
      results.push({ backend, model, passed: false, error: error.message });
    } finally {
      if (started) {
        try {
          await run("server-off", ["--server", "off"]);
          requireOutput((await run("server-stopped", ["--server", "status"])).stdout, "Server: stopped");
        } catch (error) { results.push({ backend, passed: false, error: `daemon cleanup: ${error.message}` }); }
      }
      await writeFile(join(fixtureRoot, "results.json"), `${JSON.stringify(results, null, 2)}\n`);
      if (temporary) await rm(temporary, { recursive: true, force: true });
    }
  }
  if (results.some((result) => !result.passed)) throw new Error(`npm entry model smoke failed; see ${join(fixtureRoot, "results.json")}`);
  console.log(`npm entry model smoke passed: ${smokeModels.map(([backend]) => backend).join(", ")}`);
  return results;
}
