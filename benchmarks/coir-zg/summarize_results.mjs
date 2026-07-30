#!/usr/bin/env node

import { readFile, readdir, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const DEFAULT_RESULTS = join(SCRIPT_DIR, "work", "results", "cosqa");

function parseArgs(argv) {
  const options = {
    results: DEFAULT_RESULTS,
    output: undefined,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    const value = argv[index + 1];
    if (argument === "--results") {
      options.results = resolve(requiredValue(argument, value));
      index += 1;
    } else if (argument === "--output") {
      options.output = resolve(requiredValue(argument, value));
      index += 1;
    } else if (argument === "--help" || argument === "-h") {
      process.stdout.write(
        "Usage: node benchmarks/coir-zg/summarize_results.mjs " +
          "[--results <path>] [--output <path>]\n",
      );
      return null;
    } else {
      throw new Error(`Unknown argument: ${argument}`);
    }
  }
  return options;
}

function requiredValue(argument, value) {
  if (!value || value.startsWith("--")) {
    throw new Error(`${argument} requires a value`);
  }
  return value;
}

function decimal(value, digits = 4) {
  return Number(value).toFixed(digits);
}

function gibibytes(bytes) {
  return decimal(bytes / 1024 ** 3, 2);
}

async function readJson(path) {
  return JSON.parse(await readFile(path, "utf-8"));
}

async function loadRows(resultsPath) {
  const rows = [];
  for (const entry of await readdir(resultsPath, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const directory = join(resultsPath, entry.name);
    let metrics;
    let index;
    try {
      [metrics, index] = await Promise.all([
        readJson(join(directory, "metrics.json")),
        readJson(join(directory, "index.json")),
      ]);
    } catch (error) {
      if (error?.code === "ENOENT") continue;
      throw error;
    }
    rows.push({
      model: metrics.model,
      metrics: metrics.metrics,
      indexSeconds: index.wall_time_ms / 1000,
      queryMilliseconds: metrics.latency_ms.mean,
      indexPeakRss: index.peak_rss_bytes,
      queryPeakRss: metrics.peak_rss_bytes,
      failedFiles: index.status?.filesFailed ?? 0,
      truncatedFragments: index.status?.fragmentsTruncated ?? 0,
    });
  }
  return rows.sort(
    (left, right) => right.metrics["nDCG@10"] - left.metrics["nDCG@10"],
  );
}

function render(rows) {
  const lines = [
    "| Model | nDCG@10 | Recall@10 | MAP@10 | MRR@10 | nDCG@100 | Recall@100 | MAP@100 | Index time | Mean query | Index peak RSS | Query peak RSS |",
    "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
  ];
  for (const row of rows) {
    lines.push(
      `| \`${row.model}\` | ${decimal(row.metrics["nDCG@10"])} | ` +
        `${decimal(row.metrics["Recall@10"])} | ` +
        `${decimal(row.metrics["MAP@10"])} | ` +
        `${decimal(row.metrics["MRR@10"])} | ` +
        `${decimal(row.metrics["nDCG@100"])} | ` +
        `${decimal(row.metrics["Recall@100"])} | ` +
        `${decimal(row.metrics["MAP@100"])} | ` +
        `${decimal(row.indexSeconds, 1)} s | ` +
        `${decimal(row.queryMilliseconds, 1)} ms | ` +
        `${gibibytes(row.indexPeakRss)} GiB | ` +
        `${gibibytes(row.queryPeakRss)} GiB |`,
    );
  }
  lines.push("");
  lines.push(
    `All rows: ${rows.reduce((sum, row) => sum + row.failedFiles, 0)} failed files, ` +
      `${rows.reduce((sum, row) => sum + row.truncatedFragments, 0)} truncated fragments.`,
  );
  return `${lines.join("\n")}\n`;
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options === null) return;
  const rows = await loadRows(options.results);
  if (rows.length === 0) {
    throw new Error(
      `No complete benchmark results found under ${options.results}`,
    );
  }
  const markdown = render(rows);
  if (options.output) {
    await writeFile(options.output, markdown);
  } else {
    process.stdout.write(markdown);
  }
}

main().catch((error) => {
  process.stderr.write(
    `${error instanceof Error ? (error.stack ?? error.message) : String(error)}\n`,
  );
  process.exitCode = 1;
});
