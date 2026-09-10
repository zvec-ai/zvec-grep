import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { cp, mkdir, mkdtemp, readFile, readdir, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import { DaemonClient } from "../../dist/client/daemon-client.js";
import { defaultCases as builtinCases } from "./default-cases.mjs";
import { loadDataset } from "./dataset.mjs";
import { locations } from "./output-locations.mjs";

// Run from repository root after npm run build. A local model cache is required;
// no remote embedding service or account configuration is inherited.
const exec = promisify(execFile);
const cli = resolve("dist/cli/index.js");
const cache = process.env.ZVEC_GREP_MODEL_CACHE;
const sourceTree = resolve(process.env.ZVEC_GREP_EVAL_SOURCE ?? "src");
const dataset = await loadDataset(
  process.env.ZVEC_GREP_EVAL_CASES,
  builtinCases,
);
const defaultCases = dataset.cases;
const model = process.env.ZVEC_GREP_EVAL_MODEL ?? "local/potion-code-16m-v2";
assert.ok(
  model.startsWith("local/"),
  "Evaluation never sends source to remote embeddings",
);
assert.ok(cache, "Set ZVEC_GREP_MODEL_CACHE to a prepared local model cache");
const temporary = await mkdtemp(join(tmpdir(), "zvec-default-eval-"));
const home = join(temporary, "home");
await mkdir(home);
const sourceSnapshot = join(temporary, "source");
await cp(sourceTree, sourceSnapshot, { recursive: true });
const probe = createServer();
await new Promise((done) => probe.listen(0, "127.0.0.1", done));
const serverUrl = `http://127.0.0.1:${probe.address().port}/mcp`;
await new Promise((done) => probe.close(done));
const env = {
  PATH: process.env.PATH,
  HOME: home,
  USERPROFILE: home,
  ZVEC_GREP_HOME: home,
  ZVEC_GREP_SERVER_URL: serverUrl,
  ZVEC_GREP_MODEL_CACHE: cache,
  ...(process.env.ZVEC_GREP_EVAL_MODEL ? { ZVEC_GREP_EMBEDDING: model } : {}),
  NO_COLOR: "1",
};
const run = (args, root, timeout = 60_000) =>
  exec(process.execPath, ["--liftoff-only", cli, ...args], {
    cwd: root,
    env,
    timeout,
    maxBuffer: 4 * 1024 * 1024,
  });
const results = [];
const roots = [];
const revision = (await exec("git", ["rev-parse", "HEAD"])).stdout.trim();
const dirty = Boolean(
  (await exec("git", ["status", "--porcelain"])).stdout.trim(),
);
const corpusHash = createHash("sha256");
async function hashTree(directory, prefix = "", hash = corpusHash, files) {
  for (const entry of (await readdir(directory, { withFileTypes: true })).sort(
    (a, b) => a.name.localeCompare(b.name),
  )) {
    const name = `${prefix}${entry.name}`;
    if (entry.isDirectory())
      await hashTree(join(directory, entry.name), `${name}/`, hash, files);
    else if (entry.isFile()) {
      files?.add(name);
      hash
        .update(name)
        .update("\0")
        .update(await readFile(join(directory, entry.name)))
        .update("\0");
    } else throw new Error(`Unsupported corpus entry: ${name}`);
  }
}
const sourceFiles = new Set();
await hashTree(sourceSnapshot, "", corpusHash, sourceFiles);
const outputFiles = new Set(
  [...sourceFiles].map((path) =>
    dataset.layout === "root" ? path : `src/${path}`,
  ),
);
const buildHash = createHash("sha256");
await hashTree(resolve("dist"), "", buildHash);
console.log(
  JSON.stringify({
    type: "environment",
    revision,
    dirty,
    buildSha256: buildHash.digest("hex"),
    corpusSha256: corpusHash.digest("hex"),
    labelsSha256: createHash("sha256")
      .update(JSON.stringify(defaultCases))
      .digest("hex"),
    node: process.version,
    model,
    modelOverride: Boolean(process.env.ZVEC_GREP_EVAL_MODEL),
    corpus: dataset.label,
    layout: dataset.layout,
    cache: "prepared",
    note: "Curated development set, not representative traffic; cold means missing index, not missing model.",
  }),
);

async function corpus(name) {
  const root = join(temporary, name);
  await cp(
    sourceSnapshot,
    dataset.layout === "root" ? root : join(root, "src"),
    { recursive: true },
  );
  roots.push(root);
  return root;
}

async function evaluate(item, root, state, variant = "default") {
  let targetLine;
  if (item.needle) {
    const lines = (await readFile(join(root, item.file), "utf8")).split("\n");
    const matches = lines.flatMap((line, index) =>
      line.includes(item.needle) ? [index + 1] : [],
    );
    assert.equal(
      matches.length,
      1,
      `Label must identify one source line: ${item.query}`,
    );
    [targetLine] = matches;
  }
  const start = performance.now();
  const { stdout, stderr } = await run(
    variant === "default" ? [item.query] : [`--${variant}`, item.query],
    root,
  );
  const hits = locations(stdout, outputFiles);
  assert.ok(
    hits.length || /No matches\.|No text matches|No searchable/.test(stdout),
    `Unrecognized output: ${stdout}`,
  );
  const match = hits.find(
    (hit) =>
      hit.file === item.file &&
      (targetLine === undefined ||
        (hit.start <= targetLine && hit.end >= targetLine)),
  );
  const success = item.file
    ? Boolean(
        match &&
        match.rank <= (item.kind === "symbol" || item.kind === "path" ? 1 : 5),
      )
    : hits.length === 0 && /No matches\./.test(stdout);
  const record = {
    type: "query",
    state,
    variant,
    query: item.query,
    kind: item.kind,
    ms: Math.round(performance.now() - start),
    success,
    expected: item.file ? { file: item.file, line: targetLine } : "no matches",
    relevantRank: match?.rank ?? null,
    hits: hits.slice(0, 10),
    stdout,
    stderr,
  };
  results.push(record);
  console.log(JSON.stringify(record));
}

try {
  // Every cold sample is the very first invocation in an isolated source copy.
  // Stop each daemon before the next sample to avoid concurrent index builders.
  for (const [index, item] of defaultCases
    .filter((item) => item.cold)
    .entries()) {
    const root = await corpus(`cold-${index}`);
    await evaluate(item, root, "cold");
    await run(["--server", "off"], root);
  }
  const root = await corpus("warm");
  const preparing = performance.now();
  // Start through the actual default interface; preparation is measured separately.
  await run(["prepare semantic search evaluation"], root);
  const client = new DaemonClient({ serverUrl, home });
  const deadline = Date.now() + 600_000;
  let indexed = false;
  let readyFiles;
  while (Date.now() < deadline) {
    const status = await client.callTool("zvec_grep_index_status", { root });
    assert.ok(
      !["failed", "cancelled"].includes(status.runtime?.job_state),
      "Background index preparation did not succeed",
    );
    if (status.indexed && status.runtime?.job_state === "succeeded") {
      indexed = true;
      readyFiles = status.persistent.files;
      break;
    }
    await new Promise((done) => setTimeout(done, 250));
  }
  assert.ok(indexed, "Background index did not become ready in 600 seconds");
  console.log(
    JSON.stringify({
      type: "preparation",
      ms: Math.round(performance.now() - preparing),
      files: readyFiles,
    }),
  );
  for (const item of defaultCases) await evaluate(item, root, "warm");
  if (process.env.ZVEC_GREP_EVAL_DIAGNOSTICS === "1") {
    for (const item of defaultCases.filter(
      (item) => item.kind.startsWith("semantic") || item.kind === "mixed",
    )) {
      for (const variant of ["fts", "vector"])
        await evaluate(item, root, "warm", variant);
    }
  }
  for (const state of ["cold", "warm"]) {
    const selected = results.filter(
      (item) => item.state === state && item.variant === "default",
    );
    const times = selected.map((item) => item.ms).sort((a, b) => a - b);
    console.log(
      JSON.stringify({
        type: "summary",
        state,
        passed: selected.filter((item) => item.success).length,
        total: selected.length,
        p50Ms: times[Math.floor(times.length / 2)],
        p95Ms: times[Math.ceil(times.length * 0.95) - 1],
        failures: selected
          .filter((item) => !item.success)
          .map((item) => item.query),
      }),
    );
  }
} finally {
  // Do not remove a corpus until its own daemon has actually stopped.
  await run(["--server", "off"], roots.at(-1) ?? home);
  await rm(temporary, { recursive: true, force: true });
}
