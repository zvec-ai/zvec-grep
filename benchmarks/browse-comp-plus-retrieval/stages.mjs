// BrowseComp has a durable, explicitly phased lifecycle; standard pilot execution
// stays owned by the common benchmark. Scoring, snapshots and metric aggregation
// below use the same shared contracts.
import assert from "node:assert/strict";
import { mkdir, readFile, realpath } from "node:fs/promises";
import { delimiter, join } from "node:path";
import { performance } from "node:perf_hooks";
import { createWriteStream } from "node:fs";
import { fileHash, writeJson } from "../zg-retrieval/core/io.mjs";
import { embeddingRuntime } from "../zg-retrieval/core/embedding.mjs";
import {
  BENCHMARK_CONCURRENCY,
  daemonPid,
  mapConcurrent,
} from "../zg-retrieval/core/performance.mjs";
import { statistics } from "../zg-retrieval/metrics/statistics.mjs";
import { scoreFileRetrieval } from "../zg-retrieval/metrics/files.mjs";
import { scoreNdcg } from "../zg-retrieval/metrics/ndcg.mjs";
import { summarizeRepeatedQuality } from "../zg-retrieval/metrics/repetitions.mjs";
import {
  parseVisibleResponse,
  validateSearchRoute,
} from "../zg-retrieval/engines/zg/parse.mjs";
import {
  freePort,
  candidateCommand,
  runCandidate,
} from "../zg-retrieval/engines/zg/run.mjs";
import { snapshotIndex } from "../zg-retrieval/engines/zg/snapshot.mjs";
import { invalidateGroupRows } from "../zg-retrieval/expansion/run.mjs";
import { DurableMonitor, measureIndex } from "./execution.mjs";
const REPETITIONS = 5;
function searchArguments(task, root, mode) {
  return {
    root,
    ...(mode === "hybrid" ? { query: task.query } : { [mode]: [task.query] }),
    limit: 10,
    autoUpdate: false,
    freshness: "eventual",
    preferSymbol: false,
  };
}

export async function runStage(pilot, group, candidate, options, report, mcp) {
  const embedding = embeddingRuntime(pilot.model);
  const groupRows = [];
  const measured = report.performance_groups[0] ?? {
    id: group.id,
    index: null,
    query: {},
  };
  if (!report.performance_groups.length)
    report.performance_groups.push(measured);
  const evidence = join(options.output, "evidence", group.id);
  await mkdir(evidence, { recursive: true });
  const root = await realpath(group.root);
  const indexTimeoutMs = pilot.config.indexTimeoutMinutes * 60_000;
  const home = join(evidence, "runtime-home");
  const opencode = join(evidence, "opencode.json");
  await mkdir(home, { recursive: true });
  if (options.phase === "index")
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
  let client, stderrLog;
  const abort = () => {
    void client?.close().catch(() => undefined);
  };
  options.signal?.addEventListener("abort", abort, { once: true });
  try {
    if (options.phase === "index") {
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
          String(group.maxFileSize ?? 1000000),
          "--iglob",
          group.indexGlob,
        ]);
        const indexed = await measureIndex(
          invocation.command,
          invocation.args,
          {
            cwd: root,
            env,
            timeout: indexTimeoutMs,
            evidence,
            signal: options.signal,
            onProgress: options.onProgress,
          },
        );
        measured.index = indexed.measurements;
        await writeJson(join(evidence, "index.json"), indexed);
      } catch (error) {
        measured.index = error.measurements ?? null;
        await writeJson(join(evidence, "index.json"), {
          ...(error.result ?? { error: error.message }),
          measurements: error.measurements ?? null,
        });
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
      report.index_progress = {
        ...report.index_progress,
        status: "success",
        completed_files: before.files,
        total_files: group.expectedIndexedFiles,
        source: "verified public ready-status",
        elapsed_ms: measured.index.total_ms,
        observed_at: new Date().toISOString(),
      };
      report.index_status = "success";
      await options.checkpoint();
      return;
    }
    assert.equal(
      report.index_status,
      "success",
      "Query requires this run's completed fresh index",
    );
    const before = JSON.parse(
      await readFile(join(evidence, "status-before/summary.json"), "utf8"),
    );
    const config = JSON.parse(await readFile(opencode, "utf8"));
    const command = config.mcp.zvec_grep.command;
    const transport = new mcp.StdioClientTransport({
      command: command[0],
      args: command.slice(1),
      cwd: root,
      env,
      stderr: "pipe",
    });
    client = new mcp.Client({ name: "zg-retrieval-pilots", version: "1.0.0" });
    await client.connect(transport, { timeout: 120_000 });
    stderrLog = createWriteStream(join(evidence, "mcp-stderr.log"));
    transport.stderr?.pipe(stderrLog);
    const listed = await client.listTools();
    assert.ok(listed.tools.some((tool) => tool.name === "zvec_grep_search"));
    for (const mode of pilot.modes) {
      options.signal?.throwIfAborted();
      report.query_mode = mode;
      await options.checkpoint();
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
      const monitor = new DurableMonitor(
        async () => [transport.pid, await daemonPid(env.ZVEC_GREP_HOME)],
        join(evidence, `query-${mode}-rss.jsonl`),
      ).start();
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
            options.signal?.throwIfAborted();
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
            row.calls.push(call);
            row.calls.sort((a, b) => a.repetition - b.repetition);
            await options.checkpoint();
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
      await options.checkpoint();
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
    report.query_status = "success";
  } catch (error) {
    if (options.phase === "index" && measured.index)
      measured.index.status = "failed";
    invalidateGroupRows(
      groupRows,
      `group integrity was not verified: ${error.message}`,
    );
    throw error;
  } finally {
    options.signal?.removeEventListener("abort", abort);
    if (client) await client.close().catch(() => undefined);
    stderrLog?.end();
    await runCandidate(candidate, ["--server", "off"], {
      cwd: root,
      env,
      timeout: 5000,
    }).catch(() => undefined);
    await writeJson(join(evidence, "performance.json"), measured);
    await options.checkpoint();
  }
}
