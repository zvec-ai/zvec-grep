import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";
import { parseArgs } from "../../dist/cli/args.js";
import {
  createServiceOptions,
  runParsedCommand,
} from "../../dist/cli/commands.js";
import { TransformersJsEmbeddingModel } from "../../dist/engine/models/backends/transformers-js.js";
import { Model2VecEmbeddingModel } from "../../dist/engine/models/backends/model2vec.js";
import { createZvecGrep } from "../../dist/index.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { deterministicVector } from "../helpers/fake-embedding.mjs";

test("Direct automatic indexing applies the index environment to Potion batches", async (t) => {
  const directory = await createTemporaryDirectory(
    t,
    "zg-implicit-concurrency-",
  );
  const root = join(directory, "repo");
  await mkdir(root);
  const previousCwd = process.cwd();
  const previousHome = process.env.ZVEC_GREP_HOME;
  const previousEmbedding = process.env.ZVEC_GREP_EMBEDDING;
  const previousConcurrency = process.env.ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY;
  const service = await createZvecGrep({ root });
  const calls = [];
  const progress = [];
  const servicePrototype = Object.getPrototypeOf(service);
  const index = servicePrototype.index;
  // Keep the CLI, missing-index detection, model selection and scheduler real.
  // Replace only model preparation/inference to avoid downloads and native work.
  t.mock.method(Model2VecEmbeddingModel.prototype, "prepare", async () => {});
  t.mock.method(
    Model2VecEmbeddingModel.prototype,
    "doEmbed",
    async function (contents, options) {
      calls.push({
        purpose: options.purpose,
        defaultConcurrency: this.info.defaultConcurrency,
      });
      return {
        vectors: contents.map((content) =>
          deterministicVector(content.text, this.info.dimension),
        ),
        truncated: [],
      };
    },
  );
  t.mock.method(servicePrototype, "index", async function (options) {
    assert.equal(this.options.embedding, "local/potion-code-16m-v2");
    return await index.call(this, {
      ...options,
      onProgress(event) {
        progress.push(event);
        options.onProgress?.(event);
      },
    });
  });
  t.mock.method(console, "error", () => {});
  t.mock.method(console, "log", () => {});
  try {
    process.env.ZVEC_GREP_HOME = join(directory, "home");
    process.env.ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY = "12";
    delete process.env.ZVEC_GREP_EMBEDDING;
    await writeFile(join(root, "answer.ts"), "export const answer = 42;\n");
    process.chdir(root);
    await runParsedCommand(parseArgs(["--mode", "direct", "answer"]));
    assert.ok(progress.some((event) => event.embedding?.maxConcurrency === 12));
    assert.ok(calls.some((call) => call.purpose === "document"));
    assert.ok(calls.some((call) => call.purpose === "query"));
    assert.ok(calls.every((call) => call.defaultConcurrency === 2));
  } finally {
    process.chdir(previousCwd);
    if (previousHome === undefined) delete process.env.ZVEC_GREP_HOME;
    else process.env.ZVEC_GREP_HOME = previousHome;
    if (previousEmbedding === undefined) delete process.env.ZVEC_GREP_EMBEDDING;
    else process.env.ZVEC_GREP_EMBEDDING = previousEmbedding;
    if (previousConcurrency === undefined)
      delete process.env.ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY;
    else
      process.env.ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY = previousConcurrency;
    await service.close();
  }
});

test("index concurrency controls indexing and refresh without changing query embeddings", async (t) => {
  const previous = process.env.ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY;
  process.env.ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY = "8";
  t.after(() => {
    if (previous === undefined)
      delete process.env.ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY;
    else process.env.ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY = previous;
  });
  // Keep the real CLI, factory, model configuration, scheduler and index. Only
  // replace native inference so the test needs neither a GPU nor a model download.
  const calls = [];
  t.mock.method(
    TransformersJsEmbeddingModel.prototype,
    "doEmbed",
    async function (contents, options) {
      calls.push({
        concurrency: this.info.defaultConcurrency,
        purpose: options.purpose,
      });
      return {
        vectors: contents.map((content) =>
          deterministicVector(content.text, this.info.dimension),
        ),
        truncated: [],
      };
    },
  );
  const directory = await createTemporaryDirectory(t, "zg-local-concurrency-");
  const root = join(directory, "repo");
  await mkdir(root);
  await writeFile(join(root, "answer.ts"), "export const answer = 42;\n");
  const parsed = parseArgs([
    "--index",
    "--embedding",
    "local/all-minilm-l6-v2",
    "--device",
    "cpu",
    "--index-embedding-concurrency",
    "2",
  ]);
  const service = await createZvecGrep(
    createServiceOptions(parsed.options, root),
  );
  t.after(() => service.close());
  const progress = [];
  await service.index({
    embeddingConcurrency: parsed.options.embeddingConcurrency,
    onProgress: (event) => progress.push(event),
  });
  assert.ok(calls.some((call) => call.purpose === "document"));
  assert.ok(calls.every((call) => call.concurrency === 2));
  assert.ok(progress.some((event) => event.embedding?.maxConcurrency === 2));

  calls.length = 0;
  await service.context({
    query: "answer",
    embeddingConcurrency: 7,
    autoUpdate: false,
  });
  assert.deepEqual(calls, [{ concurrency: 1, purpose: "query" }]);

  calls.length = 0;
  await writeFile(
    join(root, "answer.ts"),
    "export const answer = 43;\nexport const explanation = 'updated';\n",
  );
  await service.context({
    query: "answer",
    embeddingConcurrency: 3,
    autoUpdate: true,
  });
  assert.ok(calls.some((call) => call.purpose === "document"));
  assert.ok(calls.some((call) => call.purpose === "query"));
  assert.ok(
    calls
      .filter((call) => call.purpose === "document")
      .every((call) => call.concurrency === 3),
  );
  assert.ok(
    calls
      .filter((call) => call.purpose === "query")
      .every((call) => call.concurrency === 1),
  );

  calls.length = 0;
  progress.length = 0;
  await service.index({
    embeddingConcurrency: 99,
    rebuild: true,
    onProgress: (event) => progress.push(event),
  });
  assert.ok(calls.length > 0);
  assert.ok(calls.every((call) => call.concurrency === 8));
  assert.ok(progress.some((event) => event.embedding?.maxConcurrency === 8));
});
