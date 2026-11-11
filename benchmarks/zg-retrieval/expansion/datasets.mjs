import assert from "node:assert/strict";
import { mkdir, readFile, realpath } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { fileHash, inside, readJson, run } from "../core/io.mjs";
import { embeddingModel } from "../core/embedding.mjs";
import { prepareCorpus } from "../core/corpus.mjs";
import { PILOT_SUITES } from "./config.mjs";
import { loadPilotGold } from "./gold.mjs";

const data = join(dirname(fileURLToPath(import.meta.url)), "data");
const MODES = ["hybrid", "fts", "vector"];

export async function loadPilot(
  name,
  { embedding = process.env.RETRIEVAL_EMBEDDING ?? "local" } = {},
) {
  assert.ok(Object.hasOwn(PILOT_SUITES, name), `unknown pilot: ${name}`);
  const config = PILOT_SUITES[name];
  const lockBytes = await readFile(join(data, config.lockFile));
  const lock = JSON.parse(lockBytes);
  assert.equal(lock.schema_version, 1);
  assert.equal(lock.suite, config.suite);
  if (name === "beir")
    lock.tasks = lock.datasets.flatMap((dataset) =>
      dataset.tasks.map((task) => ({
        ...task,
        id: `${dataset.id}/${task.id}`,
        source_id: task.id,
        dataset: dataset.id,
      })),
    );
  assert.equal(lock.tasks.length, config.taskCount);
  assert.equal(
    new Set(lock.tasks.map((task) => task.id)).size,
    config.taskCount,
  );
  assert.equal(lock.model, config.model);
  for (const task of lock.tasks) {
    assert.ok(typeof task.query === "string" && task.query.trim());
    if (config.targetKind === "qrels") {
      assert.ok(task.qrels.length > 0);
      assert.ok(task.qrels.every((row) => row.relevance > 0));
    } else {
      assert.equal(config.targetKind, "positive_units");
      assert.match(task.revision, /^[a-f0-9]{40}$/);
      assert.ok(task.positive_units.length > 0);
      assert.ok(task.repository && task.language);
      assert.ok(Object.hasOwn(config.fileExtensions, task.language));
      assert.ok(
        task.positive_units.every((unit) => unit.revision === task.revision),
      );
      assert.ok(
        task.positive_units.every((unit) =>
          unit.path.endsWith(`.${config.fileExtensions[task.language]}`),
        ),
      );
    }
  }
  return {
    name,
    config,
    lock,
    gold: await loadPilotGold(name, lock, lockBytes),
    model: embeddingModel(lock.model, embedding),
    modes: MODES,
  };
}

export function targetsForTask(pilot, task) {
  if (pilot.config.targetKind === "qrels")
    return task.qrels.map((row) => ({ path: `docs/${row.document_id}.md` }));
  if (pilot.config.targetKind === "positive_units")
    return [...new Set(task.positive_units.map((unit) => unit.path))].map(
      (path) => ({ path }),
    );
  assert.fail(`unknown target kind: ${pilot.config.targetKind}`);
}

async function download(url, destination, expectedHash) {
  await mkdir(dirname(destination), { recursive: true });
  await run("curl", [
    "--fail",
    "--location",
    "--silent",
    "--show-error",
    "--retry",
    "3",
    "--retry-delay",
    "1",
    url,
    "--output",
    destination,
  ]);
  assert.equal(
    await fileHash(destination),
    expectedHash,
    `source hash mismatch: ${url}`,
  );
}

function jsonl(content) {
  return content
    .trimEnd()
    .split("\n")
    .map((line) => JSON.parse(line));
}

export async function prepareBeirDataset(dataset, directory) {
  const source = join(directory, "source", "beir");
  for (const part of ["corpus", "queries"]) {
    const filename = `${part}-00000-of-00001.parquet`;
    await download(
      `https://huggingface.co/datasets/${dataset.mirror.dataset}/resolve/${dataset.mirror.revision}/${part}/${filename}`,
      join(source, dataset.id, filename),
      dataset.mirror[`${part}_parquet_sha256`],
    );
  }
  if (dataset.id !== "scifact")
    await download(
      `https://huggingface.co/datasets/${dataset.mirror.qrels_dataset}/resolve/${dataset.mirror.qrels_revision}/test.tsv`,
      join(source, dataset.id, "test.tsv"),
      dataset.mirror.qrels_test_sha256,
    );
  const output = join(directory, `beir-${dataset.id}-groups.json`);
  await run(
    "python3",
    [
      join(dirname(fileURLToPath(import.meta.url)), "prepare-beir.py"),
      "--lock",
      join(data, PILOT_SUITES.beir.lockFile),
      "--dataset",
      dataset.id,
      "--source",
      source,
      "--scifact-qrels",
      join(data, "scifact-test.tsv"),
      "--corpus",
      join(directory, "corpus"),
      "--output",
      output,
    ],
    { timeout: 900_000 },
  );
  return await readJson(output);
}

export async function prepareDuRetrieval(pilot, directory) {
  const { lock } = pilot;
  const source = join(directory, "source", "duretrieval");
  for (const [name, file] of Object.entries(lock.source.files)) {
    await download(
      `https://huggingface.co/datasets/${file.dataset}/resolve/${file.revision}/${file.path}`,
      join(source, `${name}.parquet`),
      file.sha256,
    );
  }
  const root = join(directory, "corpus", "duretrieval");
  await run(
    "python3",
    [
      join(dirname(fileURLToPath(import.meta.url)), "prepare-duretrieval.py"),
      "--lock",
      join(data, pilot.config.lockFile),
      "--source",
      source,
      "--root",
      root,
    ],
    { timeout: 900_000 },
  );
  return [
    {
      id: "duretrieval",
      root,
      tasks: lock.tasks.map((task) => ({
        id: task.id,
        query: task.query,
        targets: targetsForTask(pilot, task),
      })),
      indexGlob: "*.md",
      expectedIndexedFiles: lock.corpus_documents,
    },
  ];
}

export async function verifyQuarrySource(pilot, directory) {
  const { lock } = pilot;
  const base = `${lock.source_url}/resolve/${lock.source_revision}/data`;
  const queryPath = join(directory, "source", "quarry-queries.jsonl");
  const goldPath = join(directory, "source", "quarry-gold-preimage.jsonl");
  await download(`${base}/queries.jsonl`, queryPath, lock.queries_sha256);
  await download(
    `${base}/gold-preimage.jsonl`,
    goldPath,
    lock.gold_preimage_sha256,
  );
  const queries = new Map(
    jsonl(await readFile(queryPath, "utf8")).map((row) => [row.query_id, row]),
  );
  const gold = new Map(
    jsonl(await readFile(goldPath, "utf8")).map((row) => [row.query_id, row]),
  );
  for (const task of lock.tasks) {
    const query = queries.get(task.id);
    const annotation = gold.get(task.id);
    assert.equal(query?.query, task.query, `changed Quarry query ${task.id}`);
    assert.equal(query?.task_id, task.source_task_id);
    assert.equal(query?.revision, task.revision);
    assert.equal(query?.repo, task.repository.replace("/", "__"));
    assert.equal(annotation?.image_stage, "preimage");
    assert.deepEqual(
      annotation?.positive_units,
      task.positive_units,
      `changed Quarry gold ${task.id}`,
    );
  }
}

export async function prepareQuarryTask(pilot, task, directory) {
  const index = pilot.lock.tasks.findIndex((row) => row.id === task.id);
  assert.ok(index >= 0);
  const repo = {
    repository: `${task.repository}/task-${index + 1}`,
    url: `https://github.com/${task.repository}.git`,
    commit: task.revision,
  };
  const root = await prepareCorpus(repo, join(directory, "corpus"));
  const targets = [];
  const seen = new Set();
  for (const unit of task.positive_units) {
    assert.equal(unit.revision, task.revision);
    assert.equal(unit.image_stage, "preimage");
    assert.ok(typeof unit.path === "string" && !unit.path.startsWith("/"));
    const path = resolve(root, unit.path);
    assert.ok(
      inside(root, path) && inside(root, await realpath(path)),
      `unsafe gold path ${unit.path}`,
    );
    const lines = (await readFile(path, "utf8")).split(/\r?\n/);
    assert.ok(unit.start_line >= 1 && unit.end_line >= unit.start_line);
    assert.ok(
      unit.end_line <= lines.length,
      `stale Quarry line span ${unit.path}`,
    );
    if (!seen.has(unit.path)) {
      targets.push({ path: unit.path });
      seen.add(unit.path);
    }
  }
  return {
    id: `task-${index + 1}`,
    root,
    tasks: [
      {
        id: task.id,
        query: task.query,
        targets,
        language: task.language,
        repository: task.repository,
      },
    ],
    indexGlob: `*.${pilot.config.fileExtensions[task.language]}`,
  };
}
