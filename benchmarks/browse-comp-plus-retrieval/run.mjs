import assert from "node:assert/strict";
import {
  mkdir,
  lstat,
  realpath,
  rm,
  rename,
  writeFile,
} from "node:fs/promises";
import { createRequire } from "node:module";
import { arch, cpus, platform, totalmem } from "node:os";
import { dirname, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { verifyPackageCache } from "../shared/rust-package-cache.mjs";
import {
  corpusManifest,
  modelArtifactManifest,
} from "../zg-retrieval/core/corpus.mjs";
import { directoryBytes } from "../zg-retrieval/core/performance.mjs";
import {
  fileHash,
  readJson,
  run,
  writeJson,
} from "../zg-retrieval/core/io.mjs";
import { packageCandidate } from "../zg-retrieval/engines/zg/run.mjs";
import { summarizePilotRows } from "../zg-retrieval/expansion/run.mjs";
import {
  summarizePerformance,
  markdownPerformance,
} from "../zg-retrieval/reports/performance.mjs";
import { runStage } from "./stages.mjs";
import { runCandidate } from "./runtime.mjs";
import { auditArm, MODES } from "./report.mjs";

const here = dirname(fileURLToPath(import.meta.url));

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

export function reportWriter(output, report) {
  let pending = Promise.resolve();
  return () => {
    pending = pending.then(async () => {
      const arm = report.arm;
      arm.calls = arm.rows.reduce((sum, row) => sum + row.calls.length, 0);
      arm.performance = summarizePerformance(arm.performance_groups);
      arm.quality = summarizePilotRows(
        arm.rows,
        MODES,
        report.dataset?.tasks.length ?? 10,
      );
      await writeJson(join(output, "report.pending.json"), report);
      await rename(
        join(output, "report.pending.json"),
        join(output, "report.json"),
      );
      await writeFile(
        join(output, "summary.md"),
        [
          `# BrowseComp+ ${report.mode}`,
          "",
          `State: **${report.status}**`,
          "",
          `Rust source: \`${report.commit}\`; harness: \`${report.harness_commit}\`.`,
          "",
          `Index: ${arm.index_status}; Query: ${arm.query_status}; calls: ${arm.calls}/150.`,
          arm.index_progress
            ? `Index progress: ${arm.index_progress.completed_files ?? "unknown"}/${arm.index_progress.total_files ?? 100195}; elapsed ${(arm.index_progress.elapsed_ms / 60000).toFixed(1)} min.`
            : "",
          report.mode === "query"
            ? "Index measurements belong to the Artifact builder, not this Query run."
            : "One complete Index is one observation.",
          "",
          markdownPerformance(arm.performance),
          "",
          ...report.errors,
        ].join("\n"),
      );
    });
    return pending;
  };
}

async function requireAbsent(path) {
  await assert.rejects(
    lstat(path),
    { code: "ENOENT" },
    "an existing index cannot be silently reused or overwritten",
  );
}

async function warmModel(candidate, modelCache, output, lock) {
  const root = join(output, "warmup");
  await mkdir(root);
  await writeFile(
    join(root, "warmup.md"),
    "BrowseComp model download warmup.\n",
  );
  await run("git", ["init", "--quiet", root]);
  const start = performance.now();
  await runCandidate(
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
      env: { ...process.env, ZVEC_GREP_HOME: join(output, "warmup-home") },
      timeout: 900000,
    },
  );
  return {
    total_ms: performance.now() - start,
    scope: "download/session warmup excluded from Index/Query",
  };
}

export async function main(args = process.argv.slice(2)) {
  const { values } = parseArgs({
    args,
    options: Object.fromEntries(
      [
        "mode",
        "dataset",
        "package",
        "identity",
        "output",
        "model-cache",
        "artifact",
        "receipt",
        "harness-commit",
      ].map((key) => [key, { type: "string" }]),
    ),
  });
  assert.ok(["build", "query"].includes(values.mode));
  for (const key of [
    "dataset",
    "package",
    "identity",
    "output",
    "harness-commit",
  ])
    assert.ok(values[key], `requires --${key}`);
  const output = resolve(values.output);
  await mkdir(dirname(output), { recursive: true });
  await mkdir(output, { recursive: false });
  const identity = await readJson(values.identity);
  const report = {
    schema_version: 1,
    kind: `browsecomp-doc-${values.mode}`,
    mode: values.mode,
    status: "incomplete",
    identity,
    commit: identity.builder_commit,
    harness_commit: values["harness-commit"],
    started_at: new Date().toISOString(),
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
    },
    arm: {
      runtime: "rust-native",
      index_status: "pending",
      query_status: "pending",
      rows: [],
      failures: [],
      index_seconds: {},
      performance_groups: [],
    },
    errors: [],
  };
  const checkpoint = reportWriter(output, report);
  const abort = new AbortController();
  const interrupt = () => abort.abort(new Error("workflow cancellation"));
  process.once("SIGINT", interrupt);
  process.once("SIGTERM", interrupt);
  let root, candidate, modelCache;
  try {
    await checkpoint();
    assert.match(report.commit, /^[a-f0-9]{40}$/);
    assert.equal(
      (
        await run("git", ["-C", resolve(here, "../.."), "rev-parse", "HEAD"])
      ).stdout.trim(),
      report.harness_commit,
    );
    const { lock, dataset } = await loadDataset(
      await realpath(resolve(values.dataset)),
    );
    report.dataset = dataset;
    report.source_lock_sha256 = await fileHash(join(here, "source.lock.json"));
    root = join(resolve(values.dataset), "corpus");
    await requireAbsent(join(root, ".zvec-grep"));
    const before = await corpusManifest(root);
    assert.equal(before.entries.length, lock.corpus.count);
    report.corpus_sha256 = before.sha256;
    report.document_count = lock.corpus.count;
    report.arm.corpus_root = root;
    report.arm.corpus_before_sha256 = before.sha256;
    await writeJson(join(output, "corpus-manifest.json"), before);
    await verifyPackageCache({
      directory: resolve(values.package),
      commit: report.commit,
    });
    candidate = await packageCandidate(
      join(resolve(values.package), "candidate.tgz"),
      output,
    );
    report.package = report.arm.package = candidate.identity;
    if (values.mode === "query") {
      assert.ok(values.artifact && values.receipt);
      const manifest = await readJson(join(values.artifact, "manifest.json"));
      assert.equal(manifest.key, identity.key);
      const restored = join(output, "restored");
      await run(
        "python",
        [
          join(here, "index_artifact.py"),
          "extract",
          "--directory",
          resolve(values.artifact),
          "--key",
          identity.key,
          "--output",
          restored,
        ],
        { timeout: 1800000 },
      );
      await rename(join(restored, "index"), join(root, ".zvec-grep"));
      modelCache = join(restored, "model");
      assert.equal(manifest.build.corpus_sha256, before.sha256);
      report.index_artifact = {
        ...(await readJson(values.receipt)),
        builder_commit: manifest.build.commit,
        builder_harness_commit: manifest.build.harness_commit,
        builder_run_url: manifest.build.run_url,
        archive_sha256: manifest.archive_sha256,
      };
      report.historical_index = {
        performance: summarizePerformance(
          manifest.build.arm.performance_groups,
        ),
        environment: manifest.build.environment,
      };
      report.arm.index_artifact = report.index_artifact;
      report.arm.expected_index_status = manifest.build.status_snapshot;
      report.arm.index_status = "restored";
    } else {
      assert.ok(values["model-cache"]);
      modelCache = resolve(values["model-cache"]);
      await mkdir(modelCache, { recursive: true });
      report.warmup = await warmModel(candidate, modelCache, output, lock);
    }
    report.model_artifacts = await modelArtifactManifest(modelCache);
    assert.equal(
      report.model_artifacts.sha256,
      lock.model_artifacts_sha256,
      "model bytes differ from the frozen Linux scout",
    );
    report.arm.model_before_sha256 = report.model_artifacts.sha256;
    await checkpoint();
    const require = createRequire(join(candidate.consumer, "package.json"));
    const { Client } = await import(
      pathToFileURL(require.resolve("@modelcontextprotocol/client")).href
    );
    const { StdioClientTransport } = await import(
      pathToFileURL(require.resolve("@modelcontextprotocol/client/stdio")).href
    );
    await runStage(
      {
        name: "browsecomp-plus",
        model: lock.model,
        modes: MODES,
        config: { indexTimeoutMinutes: 180 },
      },
      {
        id: "full-corpus",
        root,
        tasks: dataset.tasks,
        indexGlob: "*.md",
        maxFileSize: lock.index_max_filesize,
        expectedIndexedFiles: lock.corpus.count,
      },
      candidate,
      {
        output,
        modelCache,
        phase: values.mode === "build" ? "index" : "query",
        signal: abort.signal,
        checkpoint,
        onProgress: async (progress) => {
          report.arm.index_progress = progress;
          await checkpoint();
        },
      },
      report.arm,
      { Client, StdioClientTransport },
    );
    report.arm.corpus_after_sha256 = (await corpusManifest(root)).sha256;
    report.arm.model_after_sha256 = (
      await modelArtifactManifest(modelCache)
    ).sha256;
    assert.equal(report.arm.corpus_after_sha256, before.sha256);
    assert.equal(report.arm.model_after_sha256, report.model_artifacts.sha256);
    if (values.mode === "query") {
      Object.assign(report.arm, await auditArm(report.arm, dataset, output));
      assert.equal(report.arm.calls, 150);
    } else {
      const stage = report.arm.performance_groups[0].index;
      assert.equal(report.arm.index_status, "success");
      assert.equal(stage.memory.status, "available");
      stage.index_bytes = await directoryBytes(join(root, ".zvec-grep"));
      assert.ok(stage.index_bytes > 0);
      report.status_snapshot = await readJson(
        join(output, "evidence/full-corpus/status-before/summary.json"),
      );
    }
    report.status = "success";
  } catch (error) {
    report.status = abort.signal.aborted ? "cancelled" : "failed";
    report.errors.push(error.message);
    console.error(error.stack);
    process.exitCode = 1;
  } finally {
    if (values.mode === "query" && root && candidate) {
      const home = join(output, "evidence/full-corpus/runtime-home");
      try {
        await runCandidate(candidate, ["--server", "off"], {
          cwd: root,
          env: {
            ...process.env,
            HOME: home,
            USERPROFILE: home,
            ZVEC_GREP_HOME: join(home, ".zvec-grep"),
          },
          timeout: 60000,
        });
        await rm(join(root, ".zvec-grep"), { recursive: true, force: true });
        await rm(join(output, "restored"), { recursive: true, force: true });
      } catch (error) {
        report.status = "failed";
        report.errors.push(`shutdown/cleanup: ${error.message}`);
        process.exitCode = 1;
      }
    }
    report.finished_at = new Date().toISOString();
    await checkpoint();
    process.removeListener("SIGINT", interrupt);
    process.removeListener("SIGTERM", interrupt);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
  await main();
