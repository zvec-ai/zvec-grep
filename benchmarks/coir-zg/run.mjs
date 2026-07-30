#!/usr/bin/env node

import { createReadStream, createWriteStream } from "node:fs";
import { mkdir, readFile, readdir, stat, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { createZvecGrep } from "../../dist/index.js";
import {
  createRemoteEmbeddingOperationPermit,
  createRemoteEmbeddingTarget,
  withRemoteEmbeddingOperationPermit,
} from "../../dist/authorization/index.js";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const PROTOCOL_VERSION = 1;
const DEFAULT_DATASET = join(SCRIPT_DIR, "work", "data", "cosqa");
const DEFAULT_OUTPUT = join(SCRIPT_DIR, "work", "results", "cosqa");

const HELP = `Usage:
  node benchmarks/coir-zg/run.mjs --model <provider/model> [options]

Options:
  --model <provider/model>   embedding model to evaluate (required)
  --dataset <path>          materialized dataset directory
  --output <path>           result directory
  --phase <phase>           all, index, or query (default: all)
  --fragment-limit <n>      fragment candidates per query (default: 500)
  --top-k <n>               deduplicated documents to retain (default: 100)
  --query-limit <n>         evaluate only the first n queries (smoke tests)
  --model-cache-dir <path>  override the zvec-grep model cache
  --help                    show this help
`;

function parseArgs(argv) {
  const options = {
    dataset: DEFAULT_DATASET,
    output: DEFAULT_OUTPUT,
    model: undefined,
    phase: "all",
    fragmentLimit: 500,
    topK: 100,
    queryLimit: undefined,
    modelCacheDir: undefined,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    const value = argv[index + 1];
    switch (argument) {
      case "--help":
      case "-h":
        process.stdout.write(HELP);
        return null;
      case "--dataset":
        options.dataset = resolve(requiredValue(argument, value));
        index += 1;
        break;
      case "--output":
        options.output = resolve(requiredValue(argument, value));
        index += 1;
        break;
      case "--model":
        if (options.model !== undefined) {
          throw new Error("--model may only be provided once per run");
        }
        options.model = requiredValue(argument, value);
        index += 1;
        break;
      case "--phase":
        options.phase = requiredValue(argument, value);
        index += 1;
        break;
      case "--fragment-limit":
        options.fragmentLimit = positiveInteger(argument, value);
        index += 1;
        break;
      case "--top-k":
        options.topK = positiveInteger(argument, value);
        index += 1;
        break;
      case "--query-limit":
        options.queryLimit = positiveInteger(argument, value);
        index += 1;
        break;
      case "--model-cache-dir":
        options.modelCacheDir = resolve(requiredValue(argument, value));
        index += 1;
        break;
      default:
        throw new Error(`Unknown argument: ${argument}`);
    }
  }

  if (options.model === undefined) {
    throw new Error("--model is required");
  }
  if (!["all", "index", "query"].includes(options.phase)) {
    throw new Error("--phase must be all, index, or query");
  }
  if (options.fragmentLimit < options.topK) {
    throw new Error("--fragment-limit must be at least --top-k");
  }
  return options;
}

function requiredValue(argument, value) {
  if (!value || value.startsWith("--")) {
    throw new Error(`${argument} requires a value`);
  }
  return value;
}

function positiveInteger(argument, value) {
  const parsed = Number.parseInt(requiredValue(argument, value), 10);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) {
    throw new Error(`${argument} must be a positive integer`);
  }
  return parsed;
}

async function readJsonLines(path) {
  const rows = [];
  const lines = createInterface({
    input: createReadStream(path, { encoding: "utf-8" }),
    crlfDelay: Infinity,
  });
  for await (const line of lines) {
    if (line.trim()) rows.push(JSON.parse(line));
  }
  return rows;
}

async function directorySize(path) {
  let total = 0;
  for (const entry of await readdir(path, { withFileTypes: true })) {
    const child = join(path, entry.name);
    if (entry.isDirectory()) total += await directorySize(child);
    else if (entry.isFile()) total += (await stat(child)).size;
  }
  return total;
}

function percentile(sorted, probability) {
  if (sorted.length === 0) return 0;
  const index = Math.ceil(sorted.length * probability) - 1;
  return sorted[Math.max(0, Math.min(index, sorted.length - 1))];
}

function evaluate(rankings, qrelsByQuery, cutoffs) {
  const totals = Object.fromEntries(
    cutoffs.flatMap((cutoff) => [
      [`nDCG@${cutoff}`, 0],
      [`Recall@${cutoff}`, 0],
      [`MAP@${cutoff}`, 0],
    ]),
  );
  totals["MRR@10"] = 0;

  for (const [queryId, relevantScores] of qrelsByQuery) {
    const ranking = rankings.get(queryId) ?? [];
    const positives = [...relevantScores.entries()]
      .filter(([, score]) => score > 0)
      .sort((left, right) => right[1] - left[1]);
    const positiveIds = new Set(positives.map(([docId]) => docId));

    for (const cutoff of cutoffs) {
      let dcg = 0;
      let hits = 0;
      let precisionSum = 0;
      for (let rank = 0; rank < Math.min(cutoff, ranking.length); rank += 1) {
        const docId = ranking[rank];
        const relevance = relevantScores.get(docId) ?? 0;
        if (relevance > 0) {
          hits += 1;
          precisionSum += hits / (rank + 1);
          dcg += (2 ** relevance - 1) / Math.log2(rank + 2);
        }
      }

      let idealDcg = 0;
      for (let rank = 0; rank < Math.min(cutoff, positives.length); rank += 1) {
        idealDcg += (2 ** positives[rank][1] - 1) / Math.log2(rank + 2);
      }

      totals[`nDCG@${cutoff}`] += idealDcg > 0 ? dcg / idealDcg : 0;
      totals[`Recall@${cutoff}`] +=
        positiveIds.size > 0 ? hits / positiveIds.size : 0;
      totals[`MAP@${cutoff}`] +=
        positiveIds.size > 0 ? precisionSum / positiveIds.size : 0;
    }

    const firstRelevantRank = ranking
      .slice(0, 10)
      .findIndex((docId) => positiveIds.has(docId));
    totals["MRR@10"] +=
      firstRelevantRank >= 0 ? 1 / (firstRelevantRank + 1) : 0;
  }

  const queryCount = qrelsByQuery.size;
  return Object.fromEntries(
    Object.entries(totals).map(([name, total]) => [
      name,
      queryCount > 0 ? total / queryCount : 0,
    ]),
  );
}

function resolveApiKey(provider) {
  const providerVariables = {
    qwen: ["DASHSCOPE_API_KEY", "QWEN_API_KEY"],
    openai: ["OPENAI_API_KEY"],
    jina: ["JINA_API_KEY"],
  };
  for (const name of [
    "ZVEC_GREP_API_KEY",
    ...(providerVariables[provider] ?? []),
  ]) {
    const value = process.env[name];
    if (value?.trim()) return value;
  }
  return undefined;
}

function environmentMetadata() {
  return {
    node: process.version,
    platform: process.platform,
    arch: process.arch,
  };
}

async function runModel(options, dataset) {
  const model = options.model;
  const slug = model.replaceAll("/", "__");
  const modelOutput = join(options.output, slug);
  const corpusRoot = join(options.dataset, "corpus");
  await mkdir(modelOutput, { recursive: true });

  const [provider, providerModel] = model.split("/", 2);
  if (!provider || !providerModel) {
    throw new Error(`Invalid model reference: ${model}`);
  }
  const remote = provider !== "local";
  const apiKey = remote ? resolveApiKey(provider) : undefined;
  if (remote && !apiKey) {
    throw new Error(
      `Missing API key for ${model}; set ZVEC_GREP_API_KEY or the provider-specific API key`,
    );
  }

  const serviceOptions = {
    root: corpusRoot,
    embedding: model,
    modelCacheDir: options.modelCacheDir,
    apiKey,
  };
  const service = await createZvecGrep(serviceOptions);
  const remotePermit = remote
    ? createRemoteEmbeddingOperationPermit(
        await createRemoteEmbeddingTarget({
          roots: [corpusRoot],
          provider,
          model: providerModel,
          serviceOptions,
        }),
        "once",
      )
    : undefined;
  const authorized = (operation) =>
    remotePermit
      ? withRemoteEmbeddingOperationPermit(remotePermit, operation)
      : operation();

  let peakRssBytes = process.memoryUsage().rss;
  const memorySampler = setInterval(() => {
    peakRssBytes = Math.max(peakRssBytes, process.memoryUsage().rss);
  }, 500);
  memorySampler.unref();

  try {
    if (options.phase !== "query") {
      let lastProgressLog = 0;
      const started = performance.now();
      const indexResult = await authorized(() =>
        service.index({
          rebuild: true,
          onProgress(progress) {
            const now = performance.now();
            if (now - lastProgressLog >= 5000) {
              process.stdout.write(
                `[${model}] index ${JSON.stringify(progress)}\n`,
              );
              lastProgressLog = now;
            }
          },
        }),
      );
      const status = (await service.info({ includeStatus: true })).status;
      const indexBytes = await directorySize(join(corpusRoot, ".zvec-grep"));
      const indexReport = {
        benchmark: "CoIR-ZG",
        protocol_version: PROTOCOL_VERSION,
        task: "cosqa",
        model,
        environment: environmentMetadata(),
        dataset: dataset.metadata,
        wall_time_ms: performance.now() - started,
        index_bytes: indexBytes,
        peak_rss_bytes: peakRssBytes,
        result: indexResult,
        status,
      };
      await writeFile(
        join(modelOutput, "index.json"),
        `${JSON.stringify(indexReport, null, 2)}\n`,
      );
      process.stdout.write(
        `[${model}] indexed ${indexResult.filesAdded} files in ${(
          indexReport.wall_time_ms / 1000
        ).toFixed(1)}s, ${indexResult.entitiesCreated} entities, ` +
          `${status?.fragmentsTruncated ?? 0} truncated fragments\n`,
      );
    }

    if (options.phase === "index") return;

    const selectedQueries =
      options.queryLimit === undefined
        ? dataset.queries
        : dataset.queries.slice(0, options.queryLimit);
    const selectedQueryIds = new Set(
      selectedQueries.map((query) => query.query_id),
    );
    const selectedQrels = new Map(
      [...dataset.qrelsByQuery].filter(([queryId]) =>
        selectedQueryIds.has(queryId),
      ),
    );

    const warmupStarted = performance.now();
    await authorized(() =>
      service.context({
        routes: [{ mode: "vector", query: selectedQueries[0].text }],
        limit: options.fragmentLimit,
        autoUpdate: false,
      }),
    );
    const warmupMs = performance.now() - warmupStarted;

    const rankings = new Map();
    const latencies = [];
    const rankingPath = join(modelOutput, "rankings.jsonl");
    const rankingStream = createWriteStream(rankingPath, {
      encoding: "utf-8",
    });

    for (const [index, query] of selectedQueries.entries()) {
      const started = performance.now();
      const result = await authorized(() =>
        service.context({
          routes: [{ mode: "vector", query: query.text }],
          limit: options.fragmentLimit,
          autoUpdate: false,
        }),
      );
      const latencyMs = performance.now() - started;
      latencies.push(latencyMs);

      const seen = new Set();
      const documents = [];
      for (const item of result.items) {
        const docId = dataset.docIdByPath.get(item.file.relativePath);
        if (!docId) {
          throw new Error(
            `Search returned an unmapped file: ${item.file.relativePath}`,
          );
        }
        if (seen.has(docId)) continue;
        seen.add(docId);
        documents.push(docId);
        if (documents.length >= options.topK) break;
      }
      rankings.set(query.query_id, documents);
      rankingStream.write(
        `${JSON.stringify({
          query_id: query.query_id,
          latency_ms: latencyMs,
          fragment_hits: result.items.length,
          document_hits: documents.length,
          documents,
        })}\n`,
      );

      if ((index + 1) % 10 === 0 || index + 1 === selectedQueries.length) {
        process.stdout.write(
          `[${model}] queried ${index + 1}/${selectedQueries.length}\n`,
        );
      }
    }
    rankingStream.end();
    await new Promise((resolveStream, rejectStream) => {
      rankingStream.on("finish", resolveStream);
      rankingStream.on("error", rejectStream);
    });

    const sortedLatencies = [...latencies].sort((left, right) => left - right);
    const metrics = evaluate(rankings, selectedQrels, [10, 100]);
    const report = {
      benchmark: "CoIR-ZG",
      protocol_version: PROTOCOL_VERSION,
      task: "cosqa",
      model,
      environment: environmentMetadata(),
      dataset: dataset.metadata,
      retrieval: "vector-only",
      evaluation_level: "document",
      fragment_limit: options.fragmentLimit,
      document_top_k: options.topK,
      query_count: selectedQueries.length,
      corpus_document_count: dataset.docIdByPath.size,
      corpus_sha256: dataset.metadata.corpus_sha256,
      metrics,
      latency_ms: {
        warmup: warmupMs,
        mean:
          latencies.reduce((total, latency) => total + latency, 0) /
          latencies.length,
        p50: percentile(sortedLatencies, 0.5),
        p95: percentile(sortedLatencies, 0.95),
        p99: percentile(sortedLatencies, 0.99),
        min: sortedLatencies[0],
        max: sortedLatencies.at(-1),
      },
      peak_rss_bytes: peakRssBytes,
    };
    await writeFile(
      join(modelOutput, "metrics.json"),
      `${JSON.stringify(report, null, 2)}\n`,
    );
    process.stdout.write(`[${model}] ${JSON.stringify(metrics)}\n`);
  } finally {
    clearInterval(memorySampler);
    await service.close();
  }
}

async function loadDataset(datasetPath) {
  const [manifest, queries, qrels, metadataText] = await Promise.all([
    readJsonLines(join(datasetPath, "manifest.jsonl")),
    readJsonLines(join(datasetPath, "queries.jsonl")),
    readJsonLines(join(datasetPath, "qrels.jsonl")),
    readFile(join(datasetPath, "metadata.json"), "utf-8"),
  ]);
  const docIdByPath = new Map(
    manifest.map((entry) => [entry.path, entry.doc_id]),
  );
  const qrelsByQuery = new Map();
  for (const qrel of qrels) {
    const relevance = qrelsByQuery.get(qrel.query_id) ?? new Map();
    relevance.set(qrel.corpus_id, qrel.score);
    qrelsByQuery.set(qrel.query_id, relevance);
  }
  return {
    docIdByPath,
    queries,
    qrelsByQuery,
    metadata: JSON.parse(metadataText),
  };
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options === null) return;

  await mkdir(options.output, { recursive: true });
  const dataset = await loadDataset(options.dataset);
  if (dataset.queries.length === 0) {
    throw new Error("Dataset has no queries");
  }
  await runModel(options, dataset);
}

main().catch((error) => {
  process.stderr.write(
    `${error instanceof Error ? (error.stack ?? error.message) : String(error)}\n`,
  );
  process.exitCode = 1;
});
