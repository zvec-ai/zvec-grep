import assert from "node:assert/strict";
import {
  cp,
  lstat,
  mkdir,
  readFile,
  writeFile,
  appendFile,
} from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { arch, cpus, platform, totalmem } from "node:os";
import { performance } from "node:perf_hooks";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import {
  NODE_RELEASE,
  verifyRetrievalPackage,
} from "../../shared/retrieval-package.mjs";
import {
  packageCandidate,
  candidateCommand,
  runCandidate,
} from "../../zg-retrieval/engines/zg/run.mjs";
import { snapshotIndex } from "../../zg-retrieval/engines/zg/snapshot.mjs";
import {
  corpusManifest,
  modelArtifactManifest,
} from "../../zg-retrieval/core/corpus.mjs";
import { directoryBytes } from "../../zg-retrieval/core/performance.mjs";
import {
  fileHash,
  readJson,
  run,
  writeJson,
} from "../../zg-retrieval/core/io.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const { values } = parseArgs({
  options: Object.fromEntries(
    [
      "samples",
      "node-package",
      "rust-package",
      "rust-commit",
      "harness-commit",
      "model-cache",
      "output",
    ].map((key) => [key, { type: "string" }]),
  ),
});
for (const key of [
  "samples",
  "node-package",
  "rust-package",
  "rust-commit",
  "harness-commit",
  "model-cache",
  "output",
])
  assert.ok(values[key], `requires --${key}`);
for (const key of ["rust-commit", "harness-commit"])
  assert.match(values[key], /^[a-f0-9]{40}$/);
assert.equal(platform(), "linux", "do not extrapolate macOS runs to Linux CI");
const output = resolve(values.output),
  samples = resolve(values.samples),
  modelCache = resolve(values["model-cache"]);
await mkdir(output, { recursive: false });
await mkdir(modelCache, { recursive: true });
const lock = await readJson(join(here, "../source.lock.json"));
const selection = await readJson(join(samples, "samples.json"));
assert.equal(lock.model, "local/potion-multilingual-128m");
assert.equal(lock.concurrency, 4);
const report = {
  schema_version: 1,
  purpose: "estimation_only_not_full_corpus_acceptance",
  status: "running",
  rust_commit: values["rust-commit"],
  harness_commit: values["harness-commit"],
  node_release: NODE_RELEASE,
  source_lock_sha256: await fileHash(join(here, "../source.lock.json")),
  samples_sha256: await fileHash(join(samples, "samples.json")),
  selection,
  model: lock.model,
  device: "cpu",
  embedding_concurrency: 4,
  query_concurrency_limit: 4,
  run_url: `${process.env.GITHUB_SERVER_URL}/${process.env.GITHUB_REPOSITORY}/actions/runs/${process.env.GITHUB_RUN_ID}`,
  environment: {
    platform: platform(),
    arch: arch(),
    cpus: cpus(),
    total_memory_bytes: totalmem(),
    node: process.version,
    runner_image: process.env.ImageVersion,
    runner_name: process.env.RUNNER_NAME,
  },
  warmups: {},
  results: [],
  errors: [],
  started_at: new Date().toISOString(),
  prior_node_observation: {
    run_id: 40642903256,
    index_seconds: 18000.418790177,
    indexed_documents: 56981,
    total_documents: 100195,
    timed_out: true,
    linear_full_hours: 8.792125356,
    limitation:
      "unfinished index, no final optimization; late-scale behavior must constrain small-sample estimates",
  },
};
for (const path of [
  "/etc/os-release",
  "/sys/fs/cgroup/cpu.max",
  "/sys/fs/cgroup/memory.max",
])
  report.environment[path] = await readFile(path, "utf8").catch(() => null);
report.environment.lscpu = (await run("lscpu", ["--json"])).stdout;

function summary() {
  const rows = [
    "# BrowseComp index estimate (short samples only)",
    "",
    `Rust source: \`${report.rust_commit}\`; harness: \`${report.harness_commit}\`.`,
    "",
    "Both products run sequentially on this Linux runner, local Model2Vec CPU, embedding concurrency 4. Download warmups excluded; Index includes CLI/model initialization, writes and final optimization.",
    "",
    "| Product | Documents | Bytes | Actual stored entities | Index seconds | docs/s | Sampled RSS GiB | CPU user+system seconds | Byte-linear full hours (scale assumption only) | Status |",
    "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
  ];
  for (const row of report.results) {
    const m = row.measurement;
    rows.push(
      `| ${row.product} | ${row.documents} | ${row.bytes} | ${row.entities ?? "unavailable"} | ${m?.total_seconds?.toFixed(3) ?? "unavailable"} | ${row.docs_per_second?.toFixed(2) ?? "unavailable"} | ${m?.rss?.peak_bytes == null ? "unavailable" : (m.rss.peak_bytes / 2 ** 30).toFixed(3)} | ${m?.wait4 ? (m.wait4.user_seconds + m.wait4.system_seconds).toFixed(3) : "unavailable"} | ${row.byte_linear_full_hours?.toFixed(3) ?? "unavailable"} | ${row.status} |`,
    );
  }
  rows.push(
    "",
    "Public status entities are the actual stored Markdown retrieval entities; the full public status is retained. Internal embedding inputs are not exposed. Rust public debug phase timers are retained without treating overlapping worker durations as additive wall time. Node phase timers are unavailable. External progress markers are receipt timestamps, not exact substage timers.",
    "",
    "Node's previous full-corpus attempt reached only 56,981/100,195 documents in 5.000 hours before SIGKILL: 8.792 hours by simple extrapolation, excluding unfinished tail/finalize. Small indexes cannot establish full-size scaling or override that observation. Rust full-size finalization remains uncertain until measured.",
    "",
    "Each sample must exit successfully after optimization and pass public ready/coverage checks. These results never satisfy the required fresh 100,195-document indexes and 150 public MCP calls per product.",
    "",
    `State: ${report.status}.`,
  );
  if (report.errors.length)
    rows.push("", ...report.errors.map((error) => `- ${error}`));
  return rows.join("\n") + "\n";
}

async function checkpoint() {
  await writeJson(join(output, "report.json"), report);
  await writeFile(join(output, "summary.md"), summary());
}

try {
  const candidates = {};
  for (const name of ["node", "rust"]) {
    const tarball = resolve(values[`${name}-package`]);
    const manifest = await verifyRetrievalPackage({
      directory: dirname(tarball),
      commit: name === "node" ? NODE_RELEASE.identity : report.rust_commit,
    });
    assert.equal(await fileHash(tarball), manifest.tarball_sha256);
    const directory = join(output, "packages", name);
    await mkdir(directory, { recursive: true });
    const candidate = (candidates[name] = await packageCandidate(
      tarball,
      directory,
    ));
    assert.equal(
      candidate.runtime,
      name === "node" ? "node-0.2.2" : "rust-native",
    );
    const root = join(output, "warmups", name);
    await mkdir(root, { recursive: true });
    await writeFile(join(root, "warm.md"), "Local model download warmup.\n");
    await run("git", ["init", "--quiet", root]);
    const started = performance.now();
    const result = await runCandidate(
      candidate,
      [
        "--index",
        root,
        "--mode",
        "direct",
        "--embedding",
        lock.model,
        "--model-cache",
        modelCache,
        "--device",
        "cpu",
        ...candidate.indexConcurrencyArguments,
        "--iglob",
        "*.md",
      ],
      {
        cwd: root,
        env: {
          ...process.env,
          ZVEC_GREP_HOME: join(output, "runtime-homes", `warm-${name}`),
        },
        timeout: 900_000,
        log: (part) => process.stdout.write(part),
      },
    );
    report.warmups[name] = {
      seconds: (performance.now() - started) / 1000,
      package: candidate.identity,
      manifest,
      result,
    };
    await checkpoint();
  }
  const expectedModel = await modelArtifactManifest(modelCache);
  report.model_artifacts = expectedModel;
  assert.ok(expectedModel.entries.length, "model artifacts are missing");
  // Alternate product order by size while keeping all indexes sequential.
  for (const [count, names] of [
    [256, ["node", "rust"]],
    [1024, ["rust", "node"]],
  ]) {
    for (const name of names) {
      const candidate = candidates[name],
        row = {
          product: name,
          documents: count,
          bytes: selection.samples[count].bytes,
          status: "running",
        };
      report.results.push(row);
      const directory = join(output, "runs", `${count}-${name}`),
        root = join(directory, "corpus");
      await mkdir(directory, { recursive: true });
      await cp(join(samples, String(count)), root, {
        recursive: true,
        errorOnExist: true,
        force: false,
      });
      await assert.rejects(lstat(join(root, ".zvec-grep")), { code: "ENOENT" });
      const before = await corpusManifest(root);
      assert.equal(before.entries.length, count);
      row.corpus_sha256 = before.sha256;
      const env = {
        ...process.env,
        ZVEC_GREP_HOME: join(output, "runtime-homes", `${count}-${name}`),
        ZVEC_GREP_MODEL_CACHE: modelCache,
        ZVEC_GREP_DEVICE: "cpu",
        NO_COLOR: "1",
        FORCE_COLOR: "0",
      };
      const command = candidateCommand(candidate, [
        "--index",
        root,
        "--mode",
        "direct",
        "--embedding",
        lock.model,
        "--model-cache",
        modelCache,
        "--device",
        "cpu",
        ...candidate.indexConcurrencyArguments,
        "--max-filesize",
        String(lock.index_max_filesize),
        "--iglob",
        "*.md",
        "--debug",
      ]);
      console.log(
        `Starting ${name}: fresh ${count} documents, ${row.bytes} bytes`,
      );
      await checkpoint();
      try {
        assert.equal(
          (await modelArtifactManifest(modelCache)).sha256,
          expectedModel.sha256,
        );
        try {
          await run(
            "python3",
            [
              join(here, "measure.py"),
              "--cwd",
              root,
              "--output",
              join(directory, "index"),
              "--timeout",
              "1200",
              "--",
              command.command,
              ...command.args,
            ],
            {
              cwd: root,
              env,
              timeout: 1230_000,
              log: (part) => process.stdout.write(part),
            },
          );
        } finally {
          row.measurement = await readJson(
            join(directory, "index/measurement.json"),
          ).catch(() => null);
        }
        assert.equal(row.measurement?.status, "success");
        const status = await snapshotIndex({
          cli: candidate.cli,
          root,
          output: join(directory, "public-status"),
          env,
          runtime: candidate.runtime,
          runStatus: (_cli, args, options) =>
            runCandidate(candidate, args, options),
        });
        assert.equal(status.files, count);
        row.entities = status.fragments;
        row.index_bytes = await directoryBytes(join(root, ".zvec-grep"));
        assert.equal((await corpusManifest(root)).sha256, before.sha256);
        assert.equal(
          (await modelArtifactManifest(modelCache)).sha256,
          expectedModel.sha256,
        );
        row.docs_per_second = count / row.measurement.total_seconds;
        row.bytes_per_second = row.bytes / row.measurement.total_seconds;
        row.byte_linear_full_hours =
          selection.full_corpus.bytes / row.bytes_per_second / 3600;
        row.status = "success";
      } catch (error) {
        row.status = "failed";
        row.error = error.message;
        report.errors.push(`${count}-${name}: ${error.message}`);
      }
      await writeJson(join(directory, "result.json"), row);
      await checkpoint();
      console.log(
        `${count}-${name}: ${row.status}; ${row.measurement?.total_seconds?.toFixed(3)}s`,
      );
      if (process.env.GITHUB_STEP_SUMMARY)
        await appendFile(
          process.env.GITHUB_STEP_SUMMARY,
          `\nCompleted ${count}-${name}: ${row.status}; Index ${row.measurement?.total_seconds?.toFixed(3) ?? "unavailable"}s; entities ${row.entities ?? "unavailable"}.\n`,
        );
    }
  }
  report.status = report.errors.length ? "failed" : "success";
} catch (error) {
  report.status = "failed";
  report.errors.push(error.stack);
} finally {
  report.finished_at = new Date().toISOString();
  await checkpoint();
}
if (report.status !== "success") process.exitCode = 1;
