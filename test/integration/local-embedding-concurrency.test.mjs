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
import { createZvecGrep } from "../../dist/index.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";
import { deterministicVector } from "../helpers/fake-embedding.mjs";

test("Direct first-query indexing forwards concurrency to the default Potion model", async (t) => {
  const directory = await createTemporaryDirectory(
    t,
    "zg-implicit-concurrency-",
  );
  const root = join(directory, "repo");
  await mkdir(root);
  const previousCwd = process.cwd();
  const previousHome = process.env.ZVEC_GREP_HOME;
  const previousEmbedding = process.env.ZVEC_GREP_EMBEDDING;
  const service = await createZvecGrep({ root });
  const calls = [];
  const sentinel = new Error("Implicit indexing reached");
  // Keep parsing, dispatch, missing-index detection and model selection real;
  // stop before indexing can load Potion or download any model artifacts.
  t.mock.method(
    Object.getPrototypeOf(service),
    "index",
    async function (options) {
      calls.push({
        embedding: this.options.embedding,
        root: options.root,
        concurrency: options.embeddingConcurrency,
      });
      throw sentinel;
    },
  );
  t.mock.method(console, "error", () => {});
  try {
    process.env.ZVEC_GREP_HOME = join(directory, "home");
    delete process.env.ZVEC_GREP_EMBEDDING;
    process.chdir(root);
    await assert.rejects(
      runParsedCommand(
        parseArgs([
          "--mode",
          "direct",
          "--embedding-concurrency",
          "3",
          "answer",
        ]),
      ),
      (error) => error === sentinel,
    );
    assert.deepEqual(calls, [
      {
        embedding: "local/potion-code-16m-v2",
        root: process.cwd(),
        concurrency: 3,
      },
    ]);
  } finally {
    process.chdir(previousCwd);
    if (previousHome === undefined) delete process.env.ZVEC_GREP_HOME;
    else process.env.ZVEC_GREP_HOME = previousHome;
    if (previousEmbedding === undefined) delete process.env.ZVEC_GREP_EMBEDDING;
    else process.env.ZVEC_GREP_EMBEDDING = previousEmbedding;
    await service.close();
  }
});

test("CLI and per-operation concurrency reach local model construction, search, and refresh", async (t) => {
  const previous = process.env.ZVEC_GREP_LOCAL_EMBEDDING_CONCURRENCY;
  process.env.ZVEC_GREP_LOCAL_EMBEDDING_CONCURRENCY = "8";
  t.after(() => {
    if (previous === undefined)
      delete process.env.ZVEC_GREP_LOCAL_EMBEDDING_CONCURRENCY;
    else process.env.ZVEC_GREP_LOCAL_EMBEDDING_CONCURRENCY = previous;
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
    "--embedding-concurrency",
    "2",
  ]);
  const service = await createZvecGrep(
    createServiceOptions(parsed.options, root),
  );
  t.after(() => service.close());
  const progress = [];
  await service.index({ onProgress: (event) => progress.push(event) });
  assert.ok(calls.some((call) => call.purpose === "document"));
  assert.ok(calls.every((call) => call.concurrency === 2));
  assert.ok(progress.some((event) => event.embedding?.maxConcurrency === 2));

  calls.length = 0;
  await service.context({
    query: "answer",
    embeddingConcurrency: 1,
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
  assert.ok(calls.every((call) => call.concurrency === 3));

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
