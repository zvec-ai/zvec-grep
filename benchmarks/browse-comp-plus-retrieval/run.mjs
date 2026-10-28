import assert from "node:assert/strict";
import { mkdir, lstat, realpath, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { arch, cpus, platform, totalmem } from "node:os";
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
import { runGroup } from "../zg-retrieval/expansion/run.mjs";
import { summarizePilotRows } from "../zg-retrieval/expansion/run.mjs";
import { summarizePerformance } from "../zg-retrieval/reports/performance.mjs";
import { auditArm, markdownReport, MODES } from "./report.mjs";

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
      ].map((key) => [key, { type: "string" }]),
    ),
  });
  for (const key of [
    "dataset",
    "node-package",
    "rust-package",
    "rust-commit",
    "harness-commit",
    "output",
  ])
    assert.ok(values[key], `requires --${key}`);
  for (const key of ["rust-commit", "harness-commit"])
    assert.match(values[key], /^[a-f0-9]{40}$/);
  const output = resolve(values.output),
    prepared = await realpath(resolve(values.dataset));
  await mkdir(output, { recursive: false });
  const report = {
    schema_version: 1,
    protocol: "browsecomp-plus-ci10-public-mcp-v1",
    status: "failed",
    rust_commit: values["rust-commit"],
    harness_commit: values["harness-commit"],
    run_url: process.env.GITHUB_RUN_ID
      ? `${process.env.GITHUB_SERVER_URL}/${process.env.GITHUB_REPOSITORY}/actions/runs/${process.env.GITHUB_RUN_ID}`
      : null,
    environment: {
      platform: platform(),
      arch: arch(),
      cpus: cpus().length,
      total_memory_bytes: totalmem(),
      node: process.version,
    },
    started_at: new Date().toISOString(),
    arms: {},
    model_download_warmups: {},
    errors: [],
  };
  try {
    const { lock, dataset } = await loadDataset(prepared);
    report.dataset = dataset;
    report.source_lock = lock;
    const root = join(prepared, "corpus");
    const expectedCorpus = await corpusManifest(root);
    assert.equal(expectedCorpus.entries.length, lock.corpus.count);
    for (const entry of expectedCorpus.entries)
      assert.equal(entry.kind, "file");
    report.corpus_sha256 = expectedCorpus.sha256;
    await writeJson(join(output, "corpus-manifest.json"), expectedCorpus);
    const modelCache = resolve(
      values["model-cache"] ?? join(output, "model-cache"),
    );
    await mkdir(modelCache, { recursive: true });
    const candidates = {};
    for (const [name, identity] of [
      ["node", NODE_RELEASE.identity],
      ["rust", values["rust-commit"]],
    ]) {
      const path = resolve(values[`${name}-package`]);
      const manifest = await verifyRetrievalPackage({
        directory: dirname(path),
        commit: identity,
      });
      assert.equal(await fileHash(path), manifest.tarball_sha256);
      const directory = join(output, name);
      await mkdir(directory);
      candidates[name] = await packageCandidate(path, directory);
      assert.equal(
        candidates[name].runtime,
        name === "node" ? "node-0.2.2" : "rust-native",
      );
      console.log(
        `Warming model artifacts for ${name} outside measured phases...`,
      );
      report.model_download_warmups[name] = await warmModel(
        candidates[name],
        name,
        modelCache,
        output,
        lock,
      );
    }
    const expectedModel = await modelArtifactManifest(modelCache);
    report.model_artifacts = expectedModel;
    for (const [name, candidate] of Object.entries(candidates)) {
      console.log(
        `Measuring ${name}: fresh ${lock.corpus.count}-document index, then 150 public MCP calls...`,
      );
      const directory = join(output, name);
      const arm = (report.arms[name] = {
        runtime: candidate.runtime,
        package: candidate.identity,
        corpus_root: root,
        rows: [],
        failures: [],
        index_seconds: {},
        performance_groups: [],
      });
      try {
        await ensureFresh(root);
        arm.corpus_before_sha256 = (await corpusManifest(root)).sha256;
        arm.model_before_sha256 = (
          await modelArtifactManifest(modelCache)
        ).sha256;
        assert.equal(arm.corpus_before_sha256, expectedCorpus.sha256);
        assert.equal(arm.model_before_sha256, expectedModel.sha256);
        const require = createRequire(join(candidate.consumer, "package.json"));
        const { Client } = await import(
          pathToFileURL(require.resolve("@modelcontextprotocol/client")).href
        );
        const { StdioClientTransport } = await import(
          pathToFileURL(require.resolve("@modelcontextprotocol/client/stdio"))
            .href
        );
        await runGroup(
          {
            name: "browsecomp-plus",
            model: lock.model,
            modes: MODES,
            config: { indexTimeoutMinutes: 300 },
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
          { output: directory, modelCache },
          arm,
          { Client, StdioClientTransport },
        );
        arm.corpus_after_sha256 = (await corpusManifest(root)).sha256;
        arm.model_after_sha256 = (
          await modelArtifactManifest(modelCache)
        ).sha256;
        Object.assign(arm, await auditArm(arm, dataset, directory));
        console.log(
          `${name}: ${arm.calls} calls verified; Index ${(arm.performance.index.total_ms.total / 1000).toFixed(2)} s.`,
        );
      } catch (error) {
        arm.failures.push({ reason: error.message });
        report.errors.push(`${name}: ${error.message}`);
        console.error(`${name}: ${error.stack}`);
      } finally {
        arm.calls = arm.rows.reduce((sum, row) => sum + row.calls.length, 0);
        arm.performance = summarizePerformance(arm.performance_groups);
        arm.quality = summarizePilotRows(arm.rows, MODES, dataset.tasks.length);
        await writeJson(join(directory, "report.json"), arm);
        // The measurement is retained in evidence; release this task's index
        // only after the product's server has stopped, before the second build.
        await rm(join(root, ".zvec-grep"), { recursive: true, force: true });
      }
    }
    report.input_identity = objectHash({
      source: lock,
      dataset: report.dataset,
      corpus: expectedCorpus.sha256,
      model: expectedModel.sha256,
    });
    report.status =
      report.errors.length === 0 && Object.keys(report.arms).length === 2
        ? "success"
        : "failed";
  } catch (error) {
    report.errors.push(error.message);
    console.error(error.stack);
  } finally {
    report.finished_at = new Date().toISOString();
    await writeJson(join(output, "summary.json"), report);
    await writeFile(join(output, "summary.md"), markdownReport(report));
  }
  if (report.status !== "success") process.exitCode = 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
  await main();
