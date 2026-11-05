import assert from "node:assert/strict";
import { mkdir, readFile, realpath, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { arch, cpus, platform } from "node:os";
import { delimiter, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { parseArgs } from "node:util";
import { fileURLToPath, pathToFileURL } from "node:url";
import { fileHash, writeJson } from "../core/io.mjs";
import { embeddingRuntime } from "../core/embedding.mjs";
import {
  BENCHMARK_CONCURRENCY,
  ResourceMonitor,
  daemonPid,
  mapConcurrent,
  measureIndex,
} from "../core/performance.mjs";
import { statistics } from "../metrics/statistics.mjs";
import {
  markdownPerformance,
  summarizePerformance,
} from "../reports/performance.mjs";
import { scoreFileRetrieval } from "../metrics/files.mjs";
import { scoreNdcg } from "../metrics/ndcg.mjs";
import { summarizeRepeatedQuality } from "../metrics/repetitions.mjs";
import {
  parseVisibleResponse,
  validateSearchRoute,
} from "../engines/zg/parse.mjs";
import {
  freePort,
  packageCandidate,
  candidateCommand,
  runCandidate,
} from "../engines/zg/run.mjs";
import { snapshotIndex } from "../engines/zg/snapshot.mjs";
import { PILOT_SUITES_BY_ID } from "./config.mjs";
import {
  loadPilot,
  prepareBeirDataset,
  prepareDuRetrieval,
  prepareQuarryTask,
  targetsForTask,
  verifyQuarrySource,
} from "./datasets.mjs";

const REPETITIONS = 5;
const LIMIT = 10;

function searchArguments(task, root, mode) {
  return {
    root,
    ...(mode === "hybrid" ? { query: task.query } : { [mode]: [task.query] }),
    limit: LIMIT,
    autoUpdate: false,
    freshness: "eventual",
    preferSymbol: false,
  };
}

function median(values) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const center = Math.floor(sorted.length / 2);
  return sorted.length % 2
    ? sorted[center]
    : (sorted[center - 1] + sorted[center]) / 2;
}

function measuredCallLatencies(row) {
  if (row.group_integrity === "failed") return [];
  return row.calls
    .filter((call) => call.status === "success")
    .map((call) => call.latency_ms);
}

export function summarizePilotRows(
  rows,
  modes = ["hybrid", "fts", "vector"],
  planned = 10,
) {
  return modes.map((mode) => {
    const selected = rows.filter((row) => row.mode === mode);
    const valid = selected.filter((row) => row.status === "success");
    const average = (select) =>
      valid.length
        ? valid.reduce((sum, row) => sum + select(row), 0) / valid.length
        : null;
    const latencies = selected.flatMap(measuredCallLatencies);
    const timing = statistics(latencies);
    return {
      mode,
      completed: valid.length,
      planned,
      metrics: {
        file_hit_at_1: average((row) => row.quality_mean.file.hit_at_1),
        file_hit_at_5: average((row) => row.quality_mean.file.hit_at_5),
        file_hit_at_10: average((row) => row.quality_mean.file.hit_at_10),
        file_mrr_at_10: average((row) => row.quality_mean.file.rr_at_10),
        ndcg_at_10: average((row) => row.quality_mean.ndcg_at_10),
      },
      ranking_repeatable_cases: valid.filter(
        (row) => row.quality_mean.ranking_repeatable,
      ).length,
      measurements: {
        output_bytes_mean: average((row) => row.output_bytes),
        output_sample_count: valid.length,
        latency_ms_mean: latencies.length
          ? latencies.reduce((sum, value) => sum + value, 0) / latencies.length
          : null,
        latency_ms_p50: median(latencies),
        latency_sample_count: latencies.length,
        latency_ms_p95: timing.p95,
        latency_ms_p99: timing.p99,
      },
    };
  });
}

export function summarizePilotBreakdown(pilot, rows) {
  const field = pilot.config.breakdownBy;
  if (!field) return [];
  const groups = [...new Set(pilot.lock.tasks.map((task) => task[field]))];
  return groups.map((name) => {
    const tasks = pilot.lock.tasks.filter((task) => task[field] === name);
    const ids = new Set(tasks.map((task) => task.id));
    return {
      name,
      summary: summarizePilotRows(
        rows.filter((row) => ids.has(row.task_id)),
        pilot.modes,
        tasks.length,
      ),
    };
  });
}

function formatNumber(value, places = 4) {
  return Number.isFinite(value) ? value.toFixed(places) : "—";
}

const tableCell = (value) =>
  String(value).replaceAll("|", "\\|").replaceAll(/\r?\n/g, " ");

function fillMissingRows(pilot, report) {
  for (const task of pilot.lock.tasks) {
    const targets = targetsForTask(pilot, task);
    for (const mode of pilot.modes) {
      let row = report.rows.find(
        (item) => item.task_id === task.id && item.mode === mode,
      );
      if (!row) {
        row = {
          task_id: task.id,
          mode,
          query: task.query,
          targets,
          calls: [],
          status: "failed",
        };
        report.rows.push(row);
      }
      if (row.status === "pending") row.status = "failed";
      if (row.status === "failed" && row.calls.length < REPETITIONS)
        row.reason =
          report.failures.find(
            (item) => item.task_id === task.id || item.task_id === "suite",
          )?.reason ?? "run stopped before this query";
    }
  }
}

export function invalidateGroupRows(rows, reason) {
  for (const row of rows) {
    row.status = "failed";
    row.group_integrity = "failed";
    row.reason = reason;
    delete row.file;
    delete row.ndcg;
    delete row.output_bytes;
    delete row.items;
    delete row.quality_mean;
  }
}

export function markdownPilotReport(report) {
  const config = PILOT_SUITES_BY_ID[report.suite];
  assert.ok(config, `unknown pilot suite: ${report.suite}`);
  const planned = report.summary[0].planned;
  const lines = [
    `## ${report.label} · ${report.status === "success" ? "✅ Complete" : "❌ Incomplete"}`,
    "",
    `${planned} original queries · model \`${report.model}\` · Rust MCP default presentation · five calls/query`,
    "",
    "| Mode | Completed | Stable Top 10 | File Hit@1 | File Hit@5 | File Hit@10 | File MRR@10 | nDCG@10 | Mean output (KiB) | Avg RT (ms) | P50 RT (ms) |",
    "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
  ];
  for (const row of report.summary) {
    const hit = (cutoff) => {
      const value = row.metrics[`file_hit_at_${cutoff}`];
      return Number.isFinite(value) ? `${(value * 100).toFixed(1)}%` : "—";
    };
    lines.push(
      `| zg-${row.mode} | ${row.completed}/${row.planned} | ${row.ranking_repeatable_cases}/${row.completed} | ${hit(1)} | ${hit(5)} | ${hit(10)} | ${formatNumber(row.metrics.file_mrr_at_10)} | ${formatNumber(row.metrics.ndcg_at_10)} | ${formatNumber(row.measurements.output_bytes_mean == null ? null : row.measurements.output_bytes_mean / 1024, 2)} | ${formatNumber(row.measurements.latency_ms_mean, 2)} | ${formatNumber(row.measurements.latency_ms_p50, 2)} |`,
    );
  }
  if (report.breakdown?.length) {
    lines.push(
      "",
      `### Scores by ${config.breakdownBy}`,
      "",
      "| Group | Mode | Completed | Stable Top 10 | File Hit@1 | File Hit@5 | File Hit@10 | File MRR@10 | nDCG@10 | Mean output (KiB) | Avg RT (ms) | P50 RT (ms) |",
      "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    );
    for (const group of report.breakdown)
      for (const row of group.summary) {
        const hit = (cutoff) => {
          const value = row.metrics[`file_hit_at_${cutoff}`];
          return Number.isFinite(value) ? `${(value * 100).toFixed(1)}%` : "—";
        };
        lines.push(
          `| ${tableCell(group.name)} | zg-${row.mode} | ${row.completed}/${row.planned} | ${row.ranking_repeatable_cases}/${row.completed} | ${hit(1)} | ${hit(5)} | ${hit(10)} | ${formatNumber(row.metrics.file_mrr_at_10)} | ${formatNumber(row.metrics.ndcg_at_10)} | ${formatNumber(row.measurements.output_bytes_mean == null ? null : row.measurements.output_bytes_mean / 1024, 2)} | ${formatNumber(row.measurements.latency_ms_mean, 2)} | ${formatNumber(row.measurements.latency_ms_p50, 2)} |`,
        );
      }
  }
  lines.push(
    "",
    markdownPerformance(summarizePerformance(report.performance_groups ?? [])),
  );
  lines.push(
    "",
    config.report.note,
    "",
    "Each completed query/mode averages five call-level quality scores. Stable Top 10 means all five ordered public result locations match exactly. Incomplete queries are listed below and are never silently scored as zero. Output is public MCP text bytes from the fifth call; Avg RT and P50 RT cover successful calls from integrity-verified groups and exclude indexing.",
  );
  lines.push(
    "",
    "<details>",
    `<summary>Per-query results (all ${planned} queries × 3 modes)</summary>`,
    "",
    "| Query | Mode | Status | Stable Top 10 | Hit@10 calls | Hit@1 | Hit@5 | Hit@10 | RR@10 | nDCG@10 | Output (KiB) | Avg RT (ms) | P50 RT (ms) |",
    "| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
  );
  for (const row of [...report.rows].sort(
    (a, b) =>
      a.task_id.localeCompare(b.task_id) || a.mode.localeCompare(b.mode),
  )) {
    const callLatencies = measuredCallLatencies(row);
    const latency = median(callLatencies);
    const latencyMean = callLatencies.length
      ? callLatencies.reduce((sum, value) => sum + value, 0) /
        callLatencies.length
      : null;
    lines.push(
      `| ${tableCell(row.task_id)} | zg-${row.mode} | ${row.status === "success" ? "✅ Scored" : `❌ ${tableCell(row.reason ?? row.calls.find((call) => call.error)?.error ?? "failed")}`} | ${row.quality_mean ? (row.quality_mean.ranking_repeatable ? "Yes" : "No") : "—"} | ${row.quality_mean ? `${row.quality_mean.hit_at_10_calls}/5` : "—"} | ${formatNumber(row.quality_mean?.file.hit_at_1)} | ${formatNumber(row.quality_mean?.file.hit_at_5)} | ${formatNumber(row.quality_mean?.file.hit_at_10)} | ${formatNumber(row.quality_mean?.file.rr_at_10)} | ${formatNumber(row.quality_mean?.ndcg_at_10)} | ${formatNumber(row.output_bytes == null ? null : row.output_bytes / 1024, 2)} | ${formatNumber(latencyMean, 2)} | ${formatNumber(latency, 2)} |`,
    );
  }
  lines.push("", "</details>");
  if (report.failures.length) {
    lines.push("", "### Failed tasks or calls", "");
    for (const failure of report.failures)
      lines.push(
        `- \`${String(failure.task_id).replaceAll("`", "")}\`: ${String(failure.reason).replaceAll(/\r?\n/g, " ")}`,
      );
  }
  return `${lines.join("\n")}\n`.replaceAll(
    "Rust MCP default presentation",
    report.runtime === "node-0.2.2"
      ? "Node.js 0.2.2 MCP default presentation"
      : "Rust MCP default presentation",
  );
}

async function writeReport(output, report, pilot) {
  report.performance = summarizePerformance(report.performance_groups);
  report.summary = summarizePilotRows(
    report.rows,
    pilot.modes,
    pilot.lock.tasks.length,
  );
  report.breakdown = summarizePilotBreakdown(pilot, report.rows);
  report.status =
    report.failures.length ||
    report.summary.some((row) => row.completed !== pilot.lock.tasks.length)
      ? "failed"
      : "success";
  report.finished_at = new Date().toISOString();
  await writeJson(join(output, "report.json"), report);
  await writeFile(join(output, "report.md"), markdownPilotReport(report));
}

export async function runGroup(pilot, group, candidate, options, report, mcp) {
  const embedding = embeddingRuntime(pilot.model);
  const groupRows = [];
  const measured = { id: group.id, index: null, query: {} };
  report.performance_groups.push(measured);
  const evidence = join(options.output, "evidence", group.id);
  await mkdir(evidence, { recursive: true });
  const root = await realpath(group.root);
  const indexTimeoutMs = pilot.config.indexTimeoutMinutes * 60_000;
  const home = join(evidence, "runtime-home");
  const opencode = join(evidence, "opencode.json");
  await mkdir(home, { recursive: true });
  await writeJson(join(home, ".zvec-grep", "config.json"), {
    version: 1,
    server: { host: "127.0.0.1", port: await freePort() },
    defaults: {
      embedding: pilot.model,
      modelCacheDir: options.modelCache,
    },
    models: embedding.remote ? {} : { [pilot.model]: { device: "cpu" } },
  });
  const env = {
    ...process.env,
    HOME: home,
    USERPROFILE: home,
    OPENCODE_CONFIG: opencode,
    ZVEC_GREP_HOME: join(home, ".zvec-grep"),
    ZVEC_GREP_MODEL_CACHE: options.modelCache,
    ...(!embedding.remote ? { ZVEC_GREP_DEVICE: "cpu" } : {}),
    NO_COLOR: "1",
    FORCE_COLOR: "0",
    PATH: `${join(candidate.consumer, "node_modules/.bin")}${delimiter}${process.env.PATH ?? ""}`,
  };
  let client;
  try {
    const install = await runCandidate(
      candidate,
      [
        "--install",
        "--target",
        "opencode",
        "--yes",
        "--mcp-transport",
        "stdio",
      ],
      { cwd: root, env },
    );
    await writeJson(join(evidence, "install.json"), install);
    const config = JSON.parse(await readFile(opencode, "utf8"));
    assert.equal(config.mcp?.zvec_grep?.type, "local");
    const command = config.mcp.zvec_grep.command;
    assert.ok(
      Array.isArray(command) &&
        command.length >= 3 &&
        command.every((part) => typeof part === "string"),
    );
    await runCandidate(candidate, ["--server", "off"], { cwd: root, env });
    const indexStart = performance.now();
    try {
      const invocation = candidateCommand(candidate, [
        "--index",
        root,
        "--mode",
        "direct",
        "--embedding",
        pilot.model,
        "--model-cache",
        options.modelCache,
        ...(!embedding.remote ? ["--device", "cpu"] : []),
        ...embedding.indexArguments,
        ...candidate.indexConcurrencyArguments,
        "--max-filesize",
        "1000000",
        "--iglob",
        group.indexGlob,
        "--debug",
      ]);
      const indexed = await measureIndex(invocation.command, invocation.args, {
        cwd: root,
        env,
        timeout: indexTimeoutMs,
      });
      measured.index = indexed.measurements;
      await writeJson(join(evidence, "index.json"), indexed);
    } catch (error) {
      measured.index = error.measurements ?? null;
      await writeJson(
        join(evidence, "index.json"),
        error.result ?? { error: error.message },
      );
      throw error;
    } finally {
      report.index_seconds[group.id] =
        (measured.index?.total_ms ?? performance.now() - indexStart) / 1000;
    }
    if (embedding.remote)
      await runCandidate(candidate, embedding.grantArguments(root), {
        cwd: root,
        env,
      });
    const before = await snapshotIndex({
      cli: candidate.cli,
      runtime: candidate.runtime,
      runStatus: (_cli, args, options) =>
        runCandidate(candidate, args, options),
      root,
      output: join(evidence, "status-before"),
      env,
    });
    assert.equal(before.failed_files.length, 0);
    assert.ok(before.files > 0);
    if (group.expectedIndexedFiles)
      assert.equal(
        before.files,
        group.expectedIndexedFiles,
        `${pilot.name} corpus was not fully indexed`,
      );
    const transport = new mcp.StdioClientTransport({
      command: command[0],
      args: command.slice(1),
      cwd: root,
      env,
      stderr: "pipe",
    });
    client = new mcp.Client({ name: "zg-retrieval-pilots", version: "1.0.0" });
    await client.connect(transport, { timeout: 120_000 });
    const listed = await client.listTools();
    assert.ok(listed.tools.some((tool) => tool.name === "zvec_grep_search"));
    for (const mode of pilot.modes) {
      const modeRows = group.tasks.map((task) => {
        const row = {
          task_id: task.id,
          mode,
          query: task.query,
          targets: task.targets,
          corpus_id: group.id,
          status: "pending",
          calls: [],
        };
        report.rows.push(row);
        groupRows.push(row);
        return { task, row };
      });
      const monitor = new ResourceMonitor(async () => [
        transport.pid,
        await daemonPid(env.ZVEC_GREP_HOME),
      ]).start();
      let completed;
      try {
        completed = await mapConcurrent(
          modeRows.flatMap(({ task, row }) =>
            Array.from({ length: REPETITIONS }, (_, index) => ({
              task,
              row,
              repetition: index + 1,
            })),
          ),
          async ({ task, row, repetition }) => {
            const args = searchArguments(task, root, mode);
            const started = performance.now();
            let response;
            let error = null;
            let parsedItems = null;
            let latency;
            try {
              response = await client.callTool(
                { name: "zvec_grep_search", arguments: args },
                undefined,
                { timeout: 120_000 },
              );
              latency = performance.now() - started;
              assert.notEqual(response.isError, true, "MCP product error");
              const parsed = parseVisibleResponse(response);
              validateSearchRoute(parsed.items, mode);
              parsedItems = parsed.items.map(
                ({ rank, path, range, matched_by }) => ({
                  rank,
                  path,
                  range,
                  matched_by,
                }),
              );
              if (repetition === REPETITIONS) {
                row.file = scoreFileRetrieval(parsed.items, task.targets);
                row.ndcg = scoreNdcg(parsed.items, task.targets);
                row.output_bytes = Buffer.byteLength(parsed.text, "utf8");
                row.items = parsedItems;
              }
            } catch (failure) {
              error = failure.message;
              response ??= {
                isError: true,
                content: [{ type: "text", text: error }],
              };
            }
            latency ??= performance.now() - started;
            const raw = join(
              evidence,
              "raw",
              `${encodeURIComponent(task.id)}-${mode}-${repetition}.json`,
            );
            await writeJson(raw, {
              request: { name: "zvec_grep_search", arguments: args },
              response,
            });
            const call = {
              repetition,
              status: error ? "failed" : "success",
              latency_ms: latency,
              items: parsedItems,
              raw_sha256: await fileHash(raw),
              error,
            };
            if (error)
              report.failures.push({
                task_id: task.id,
                mode,
                repetition,
                reason: error,
              });
            return { row, call };
          },
        );
      } finally {
        measured.query[mode] = await monitor.stop();
        measured.query[mode].concurrency = BENCHMARK_CONCURRENCY;
        measured.query[mode].status = "failed";
      }
      measured.query[mode].status = "success";
      measured.query[mode].samples_ms = completed.map(
        ({ call }) => call.latency_ms,
      );
      measured.query[mode].latency_ms = statistics(
        measured.query[mode].samples_ms,
      );
      measured.query[mode].failed_calls = completed.filter(
        ({ call }) => call.status === "failed",
      ).length;
      for (const { row, call } of completed) row.calls.push(call);
      for (const { row, task } of modeRows) {
        row.status =
          row.calls.length === REPETITIONS &&
          row.calls.every((call) => call.status === "success") &&
          row.file &&
          row.ndcg
            ? "success"
            : "failed";
        if (row.status === "success")
          row.quality_mean = summarizeRepeatedQuality(row.calls, task.targets);
      }
    }
    const after = await snapshotIndex({
      cli: candidate.cli,
      runtime: candidate.runtime,
      runStatus: (_cli, args, options) =>
        runCandidate(candidate, args, options),
      root,
      output: join(evidence, "status-after"),
      env,
    });
    assert.equal(
      after.logical_content_sha256,
      before.logical_content_sha256,
      "index changed during retrieval",
    );
    for (const row of groupRows) row.group_integrity = "verified";
  } catch (error) {
    invalidateGroupRows(
      groupRows,
      `group integrity was not verified: ${error.message}`,
    );
    throw error;
  } finally {
    if (client) await client.close().catch(() => undefined);
    await runCandidate(candidate, ["--server", "off"], {
      cwd: root,
      env,
    }).catch(() => undefined);
    await writeJson(join(evidence, "performance.json"), measured);
  }
}

export async function main(args = process.argv.slice(2)) {
  const { values } = parseArgs({
    args,
    options: {
      suite: { type: "string" },
      package: { type: "string" },
      output: { type: "string" },
      "model-cache": { type: "string" },
      "candidate-commit": { type: "string", default: "unrecorded" },
    },
  });
  assert.ok(
    values.suite && values.package && values.output,
    "requires --suite, --package and --output",
  );
  const pilot = await loadPilot(values.suite);
  const output = resolve(values.output);
  await mkdir(output, { recursive: false });
  const options = {
    output,
    modelCache: resolve(values["model-cache"] ?? join(output, "model-cache")),
  };
  await mkdir(options.modelCache, { recursive: true });
  const report = {
    schema_version: 2,
    quality_aggregation: "mean_of_five",
    suite: pilot.lock.suite,
    label: pilot.config.report.title,
    model: pilot.model,
    candidate_commit: values["candidate-commit"],
    environment: {
      platform: platform(),
      architecture: arch(),
      cpus: cpus().length,
    },
    started_at: new Date().toISOString(),
    rows: [],
    failures: [],
    index_seconds: {},
    performance_groups: [],
  };
  try {
    const candidate = await packageCandidate(values.package, output);
    report.package = candidate.identity;
    report.runtime = candidate.runtime;
    const require = createRequire(join(candidate.consumer, "package.json"));
    const mcp = {
      Client: (
        await import(
          pathToFileURL(require.resolve("@modelcontextprotocol/client")).href
        )
      ).Client,
      StdioClientTransport: (
        await import(
          pathToFileURL(require.resolve("@modelcontextprotocol/client/stdio"))
            .href
        )
      ).StdioClientTransport,
    };
    if (pilot.name === "beir") {
      for (const dataset of pilot.lock.datasets) {
        let groups;
        try {
          groups = await prepareBeirDataset(dataset, output);
        } catch (error) {
          for (const task of dataset.tasks)
            report.failures.push({
              task_id: `${dataset.id}/${task.id}`,
              reason: error.message,
            });
          console.error(`${dataset.id}: ${error.message}`);
          continue;
        }
        for (const group of groups)
          try {
            await runGroup(pilot, group, candidate, options, report, mcp);
          } catch (error) {
            for (const task of group.tasks)
              report.failures.push({ task_id: task.id, reason: error.message });
            console.error(`${group.id}: ${error.message}`);
          }
      }
    } else if (pilot.name === "duretrieval") {
      for (const group of await prepareDuRetrieval(pilot, output))
        try {
          await runGroup(pilot, group, candidate, options, report, mcp);
        } catch (error) {
          for (const task of group.tasks)
            report.failures.push({ task_id: task.id, reason: error.message });
          console.error(`${group.id}: ${error.message}`);
        }
    } else {
      await verifyQuarrySource(pilot, output);
      for (const task of pilot.lock.tasks) {
        try {
          const group = await prepareQuarryTask(pilot, task, output);
          await runGroup(pilot, group, candidate, options, report, mcp);
        } catch (error) {
          report.failures.push({ task_id: task.id, reason: error.message });
          console.error(`${task.id}: ${error.message}`);
        }
      }
    }
  } catch (error) {
    report.failures.push({ task_id: "suite", reason: error.message });
    console.error(`${pilot.name}: ${error.stack}`);
  }
  fillMissingRows(pilot, report);
  await writeReport(output, report, pilot);
  console.log(`Report: ${join(output, "report.md")}`);
  if (report.status !== "success") process.exitCode = 1;
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
)
  await main();
