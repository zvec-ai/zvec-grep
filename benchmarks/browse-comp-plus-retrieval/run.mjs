import assert from "node:assert/strict";
import {
  mkdir,
  readFile,
  lstat,
  realpath,
  rm,
  writeFile,
  rename,
} from "node:fs/promises";
import { createRequire } from "node:module";
import { arch, cpus, platform, totalmem, hostname } from "node:os";
import { dirname, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import {
  verifyRetrievalPackage,
  NODE_RELEASE,
} from "../shared/retrieval-package.mjs";
import {
  corpusManifest,
  modelArtifactManifest,
} from "../zg-retrieval/core/corpus.mjs";
import {
  fileHash,
  objectHash,
  readJson,
  run,
  writeJson,
} from "../zg-retrieval/core/io.mjs";
import {
  packageCandidate,
  runCandidate,
} from "../zg-retrieval/engines/zg/run.mjs";
import { runStage } from "./stages.mjs";
import { summarizePilotRows } from "../zg-retrieval/expansion/run.mjs";
import { summarizePerformance } from "../zg-retrieval/reports/performance.mjs";
import { auditArm, markdownReport, MODES } from "./report.mjs";

const here = dirname(fileURLToPath(import.meta.url));

async function hostIdentity() {
  const machine =
    process.platform === "linux"
      ? await readFile("/etc/machine-id", "utf8")
      : hostname();
  return objectHash({
    machine,
    hostname: hostname(),
    platform: platform(),
    arch: arch(),
    cpu: cpus()[0]?.model,
    memory: totalmem(),
  });
}

async function verifyHarness(commit) {
  assert.equal(
    (
      await run("git", ["-C", resolve(here, "../.."), "rev-parse", "HEAD"])
    ).stdout.trim(),
    commit,
    "the running harness differs from the frozen commit",
  );
  await run("git", [
    "-C",
    resolve(here, "../.."),
    "diff",
    "--exit-code",
    "HEAD",
    "--",
    "benchmarks/browse-comp-plus-retrieval",
    "benchmarks/zg-retrieval",
    "benchmarks/shared",
  ]);
}

export async function loadDataset(directory) {
  const lock = await readJson(join(here, "source.lock.json"));
  const selection = await readJson(join(here, "suites/ci-selection.json"));
  const dataset = await readJson(join(directory, "dataset.json"));
  assert.equal(dataset.schema_version, 1);
  assert.equal(
    dataset.source_lock_sha256,
    await fileHash(join(here, "source.lock.json")),
  );
  assert.equal(
    dataset.selection_sha256,
    await fileHash(join(here, "suites/ci-selection.json")),
  );
  assert.equal(dataset.corpus.count, lock.corpus.count);
  assert.equal(
    dataset.corpus.manifest_sha256,
    await fileHash(join(directory, "corpus-manifest.jsonl")),
  );
  assert.deepEqual(
    dataset.tasks.map((task) => task.id),
    selection.cases.map((row) => row.query_id),
  );
  for (const [i, task] of dataset.tasks.entries()) {
    assert.deepEqual(task.gold_docids, selection.cases[i].gold_docids);
    assert.deepEqual(task.evidence_docids, selection.cases[i].evidence_docids);
    assert.deepEqual(
      task.targets,
      task.gold_docids.map((id) => ({ path: `${id}.md` })),
    );
    assert.ok(typeof task.query === "string" && task.query.trim());
  }
  return { lock, dataset };
}

async function ensureFresh(root) {
  try {
    await lstat(join(root, ".zvec-grep"));
    assert.fail("measurement requires a fresh index, found .zvec-grep");
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
}

async function warmModel(candidate, name, modelCache, output, lock) {
  const root = join(output, "warmup", name);
  await mkdir(root, { recursive: true });
  await writeFile(
    join(root, "warmup.md"),
    "BrowseComp retrieval model download warmup.\n",
  );
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
        ZVEC_GREP_HOME: join(output, "warmup-home", name),
      },
      timeout: 900_000,
    },
  );
  const total_ms = performance.now() - started;
  await writeJson(join(output, "warmup", `${name}.json`), {
    total_ms,
    ...result,
  });
  return {
    total_ms,
    status: "success",
    scope: "download/session warmup excluded from measured Index/Query",
  };
}

async function atomicJson(path, value) {
  const temporary = `${path}.pending`;
  await writeJson(temporary, value);
  await rename(temporary, path);
}

export function reportWriter(output, report, state) {
  let pending = Promise.resolve();
  return () => {
    pending = pending.then(async () => {
      for (const arm of Object.values(report.arms)) {
        arm.calls = arm.rows.reduce((sum, row) => sum + row.calls.length, 0);
        arm.performance = summarizePerformance(arm.performance_groups);
        arm.quality = summarizePilotRows(
          arm.rows,
          MODES,
          report.dataset?.tasks.length ?? 10,
        );
      }
      await atomicJson(join(output, "state.json"), state);
      await atomicJson(join(output, "summary.json"), report);
      await writeFile(
        join(output, "summary.md.pending"),
        markdownReport(report),
      );
      await rename(
        join(output, "summary.md.pending"),
        join(output, "summary.md"),
      );
    });
    return pending;
  };
}

async function prepare(values, output, report, state, checkpoint) {
  for (const key of [
    "dataset",
    "node-package",
    "rust-package",
    "rust-commit",
    "harness-commit",
  ])
    assert.ok(values[key], `requires --${key}`);
  for (const key of ["rust-commit", "harness-commit"])
    assert.match(values[key], /^[a-f0-9]{40}$/);
  await verifyHarness(values["harness-commit"]);
  state.prepared = await realpath(resolve(values.dataset));
  const { lock, dataset } = await loadDataset(state.prepared);
  report.dataset = dataset;
  report.source_lock = lock;
  state.root = join(state.prepared, "corpus");
  await ensureFresh(state.root);
  const expected = await corpusManifest(state.root);
  assert.equal(expected.entries.length, lock.corpus.count);
  for (const entry of expected.entries) assert.equal(entry.kind, "file");
  state.corpus_sha256 = report.corpus_sha256 = expected.sha256;
  await writeJson(join(output, "corpus-manifest.json"), expected);
  state.modelCache = resolve(
    values["model-cache"] ?? join(output, "model-cache"),
  );
  await mkdir(state.modelCache, { recursive: true });
  for (const [name, identity] of [
    ["node", NODE_RELEASE.identity],
    ["rust", report.rust_commit],
  ]) {
    const arm = (report.arms[name] = {
      status: "pending",
      index_status: "pending",
      query_status: "pending",
      corpus_root: state.root,
      rows: [],
      failures: [],
      index_seconds: {},
      performance_groups: [],
    });
    await checkpoint();
    const path = resolve(values[`${name}-package`]);
    const manifest = await verifyRetrievalPackage({
      directory: dirname(path),
      commit: identity,
    });
    assert.equal(await fileHash(path), manifest.tarball_sha256);
    const directory = join(output, name);
    await mkdir(directory);
    const candidate = (state.candidates[name] = await packageCandidate(
      path,
      directory,
    ));
    assert.equal(
      candidate.runtime,
      name === "node" ? "node-0.2.2" : "rust-native",
    );
    arm.runtime = candidate.runtime;
    arm.package = candidate.identity;
    console.log(`Warming ${name} model outside measured Index/Query`);
    report.model_download_warmups[name] = await warmModel(
      candidate,
      name,
      state.modelCache,
      output,
      lock,
    );
    await checkpoint();
  }
  report.model_artifacts = await modelArtifactManifest(state.modelCache);
  report.input_identity = objectHash({
    source: lock,
    dataset,
    corpus: state.corpus_sha256,
    model: report.model_artifacts.sha256,
  });
  state.prepared_ok = true;
  await checkpoint();
}

async function stage(name, phase, output, report, state, checkpoint, signal) {
  assert.ok(state.prepared_ok, "preparation did not finish");
  const arm = report.arms[name];
  const candidate = state.candidates[name];
  const directory = join(output, name);
  assert.equal(
    arm[`${phase}_status`],
    "pending",
    "each stage is single-use; restart the workflow with a fresh output directory",
  );
  if (phase === "index") {
    await ensureFresh(state.root);
    assert.equal(
      state.index_owner,
      null,
      "previous index has not been released",
    );
    arm.corpus_before_sha256 = (await corpusManifest(state.root)).sha256;
    arm.model_before_sha256 = (
      await modelArtifactManifest(state.modelCache)
    ).sha256;
    assert.equal(arm.corpus_before_sha256, state.corpus_sha256);
    assert.equal(arm.model_before_sha256, report.model_artifacts.sha256);
    state.index_owner = name;
  } else {
    assert.equal(state.index_owner, name);
    assert.equal(arm.index_status, "success");
  }
  arm[`${phase}_status`] = "running";
  report.phase = `${name}-${phase}`;
  await checkpoint();
  const require = createRequire(join(candidate.consumer, "package.json"));
  const { Client } = await import(
    pathToFileURL(require.resolve("@modelcontextprotocol/client")).href
  );
  const { StdioClientTransport } = await import(
    pathToFileURL(require.resolve("@modelcontextprotocol/client/stdio")).href
  );
  try {
    await runStage(
      {
        name: "browsecomp-plus",
        model: report.source_lock.model,
        modes: MODES,
        config: { indexTimeoutMinutes: name === "node" ? 720 : 180 },
      },
      {
        id: "full-corpus",
        root: state.root,
        tasks: report.dataset.tasks,
        indexGlob: "*.md",
        maxFileSize: report.source_lock.index_max_filesize,
        expectedIndexedFiles: report.source_lock.corpus.count,
      },
      candidate,
      {
        output: directory,
        modelCache: state.modelCache,
        phase,
        signal,
        checkpoint,
        onProgress: async (progress) => {
          arm.index_progress = progress;
          await checkpoint();
        },
      },
      arm,
      { Client, StdioClientTransport },
    );
    if (phase === "query") {
      arm.corpus_after_sha256 = (await corpusManifest(state.root)).sha256;
      arm.model_after_sha256 = (
        await modelArtifactManifest(state.modelCache)
      ).sha256;
      assert.equal(arm.corpus_after_sha256, state.corpus_sha256);
      assert.equal(arm.model_after_sha256, report.model_artifacts.sha256);
      Object.assign(arm, await auditArm(arm, report.dataset, directory));
      arm.status = "success";
    }
    arm[`${phase}_status`] = "success";
  } catch (error) {
    arm[`${phase}_status`] = signal.aborted ? "cancelled" : "failed";
    arm.status = "failed";
    arm.failures.push({ phase, reason: error.message });
    throw error;
  } finally {
    await writeJson(join(directory, "report.json"), arm);
    await checkpoint();
  }
}

async function release(name, report, state, checkpoint) {
  if (state.index_owner !== name) return;
  const home = join(state.output, name, "evidence/full-corpus/runtime-home");
  // Only remove the index after the public shutdown has completed successfully.
  await runCandidate(state.candidates[name], ["--server", "off"], {
    cwd: state.root,
    env: {
      ...process.env,
      HOME: home,
      USERPROFILE: home,
      ZVEC_GREP_HOME: join(home, ".zvec-grep"),
    },
    timeout: 60_000,
  });
  await rm(join(state.root, ".zvec-grep"), { recursive: true, force: true });
  state.index_owner = null;
  report.arms[name].index_released = true;
  await checkpoint();
}

export async function main(args = process.argv.slice(2)) {
  const { values } = parseArgs({
    args,
    options: Object.fromEntries(
      [
        "dataset",
        "node-package",
        "rust-package",
        "rust-commit",
        "harness-commit",
        "model-cache",
        "output",
        "phase",
        "pr-number",
      ].map((key) => [key, { type: "string" }]),
    ),
  });
  assert.ok(values.output, "requires --output");
  const phase = values.phase ?? "all";
  assert.ok(
    [
      "all",
      "prepare",
      "node-index",
      "node-query",
      "rust-index",
      "rust-query",
      "release-node",
      "release-rust",
      "finish",
    ].includes(phase),
  );
  const output = resolve(values.output);
  let report, state;
  if (["all", "prepare"].includes(phase)) {
    await mkdir(output, { recursive: false });
    report = {
      schema_version: 2,
      protocol: "browsecomp-plus-ci10-public-mcp-v1",
      status: "incomplete",
      rust_commit: values["rust-commit"],
      harness_commit: values["harness-commit"],
      pr_number: values["pr-number"] ? Number(values["pr-number"]) : null,
      run_url: process.env.GITHUB_RUN_ID
        ? `${process.env.GITHUB_SERVER_URL}/${process.env.GITHUB_REPOSITORY}/actions/runs/${process.env.GITHUB_RUN_ID}`
        : null,
      environment: {
        platform: platform(),
        arch: arch(),
        cpus: cpus().length,
        cpu_model: cpus()[0]?.model,
        total_memory_bytes: totalmem(),
        node: process.version,
        host_identity_sha256: await hostIdentity(),
        runner_name: process.env.RUNNER_NAME ?? null,
        runner_environment: process.env.RUNNER_ENVIRONMENT ?? "local",
      },
      started_at: new Date().toISOString(),
      arms: {},
      model_download_warmups: {},
      errors: [],
    };
    state = {
      output,
      candidates: {},
      index_owner: null,
      prepared_ok: false,
      input_harness: report.harness_commit,
    };
  } else {
    report = await readJson(join(output, "summary.json"));
    state = await readJson(join(output, "state.json"));
    assert.equal(state.output, output);
    assert.equal(state.input_harness, report.harness_commit);
    assert.equal(
      report.environment.host_identity_sha256,
      await hostIdentity(),
      "both arms must execute on the same dedicated host",
    );
  }
  const checkpoint = reportWriter(output, report, state);
  const controller = new AbortController();
  const interrupt = () => controller.abort(new Error("workflow cancellation"));
  process.once("SIGINT", interrupt);
  process.once("SIGTERM", interrupt);
  let failed = false;
  const perform = async (selected) => {
    controller.signal.throwIfAborted();
    if (selected !== "prepare") await verifyHarness(report.harness_commit);
    report.phase = selected;
    await checkpoint();
    if (selected === "prepare")
      return prepare(values, output, report, state, checkpoint);
    if (selected.startsWith("release-"))
      return release(selected.slice(8), report, state, checkpoint);
    if (selected === "finish") {
      assert.equal(
        state.index_owner,
        null,
        "an index is still owned by an unfinished stage",
      );
      for (const name of ["node", "rust"]) {
        const arm = report.arms[name];
        assert.equal(arm?.status, "success", `${name} is incomplete`);
        await auditArm(arm, report.dataset, join(output, name));
      }
      assert.equal(report.errors.length, 0);
      report.status = "success";
      report.finished_at = new Date().toISOString();
      return;
    }
    const [name, kind] = selected.split("-");
    return stage(
      name,
      kind,
      output,
      report,
      state,
      checkpoint,
      controller.signal,
    );
  };
  try {
    await checkpoint();
    const phases =
      phase === "all"
        ? [
            "prepare",
            "node-index",
            "node-query",
            "release-node",
            "rust-index",
            "rust-query",
            "release-rust",
            "finish",
          ]
        : [phase];
    for (const selected of phases) {
      try {
        await perform(selected);
      } catch (error) {
        failed = true;
        report.status = controller.signal.aborted ? "cancelled" : "failed";
        report.errors.push(`${selected}: ${error.message}`);
        console.error(error.stack);
        if (
          controller.signal.aborted ||
          selected === "prepare" ||
          phase !== "all"
        )
          break;
      }
    }
  } finally {
    await checkpoint();
    process.removeListener("SIGINT", interrupt);
    process.removeListener("SIGTERM", interrupt);
  }
  if (failed || (phase === "finish" && report.status !== "success"))
    process.exitCode = 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
  await main();
