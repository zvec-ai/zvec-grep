import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import test from "node:test";
import {
  Model2VecEmbeddingModel,
  setModel2VecRuntimeForTesting,
} from "../../dist/engine/models/providers/model2vec/embedding.js";
import { createTemporaryDirectory } from "../helpers/fixtures.mjs";

function entry(overrides = {}) {
  return {
    backend: "model2vec",
    id: "local/test-potion",
    provider: "local",
    model: "test-potion",
    repo: "test/potion",
    revision: "0123456789abcdef",
    modelFile: "model.safetensors",
    embeddingTensor: "embeddings",
    tokenizerFile: "tokenizer.json",
    dimension: 3,
    metric: "cosine",
    normalize: true,
    queryPrefix: "query: ",
    documentPrefix: "passage: ",
    maxInputTokens: 512,
    maxBatchSize: 32,
    ...overrides,
  };
}

async function writeSafetensors(path, dtype, values, shape) {
  const bytesPerValue = dtype === "F16" ? 2 : 4;
  let header = JSON.stringify({
    embeddings: {
      dtype,
      shape,
      data_offsets: [0, values.length * bytesPerValue],
    },
  });
  while (Buffer.byteLength(header) % 8 !== 0) {
    header += " ";
  }

  const headerBytes = Buffer.from(header);
  const prefix = Buffer.alloc(8);
  prefix.writeBigUInt64LE(BigInt(headerBytes.length));
  const data = Buffer.alloc(values.length * bytesPerValue);
  values.forEach((value, index) => {
    if (dtype === "F16") {
      data.writeUInt16LE(value, index * bytesPerValue);
    } else {
      data.writeFloatLE(value, index * bytesPerValue);
    }
  });

  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, Buffer.concat([prefix, headerBytes, data]));
}

test("Model2Vec downloads pinned Safetensors assets and performs normalized static lookup", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-model2vec-");
  const calls = { downloads: [], tokenizerLoads: [], tableLoads: [] };
  const tokenizer = Object.assign(
    async (text, options) => {
      calls.tokenizerCalls ??= [];
      calls.tokenizerCalls.push({ text, options });
      return {
        input_ids: {
          data: BigInt64Array.from(
            text.includes("unknown-only")
              ? [99n]
              : text.includes("both")
                ? [0n, 1n]
                : [2n],
          ),
        },
      };
    },
    { unk_token_id: 99 },
  );
  setModel2VecRuntimeForTesting({
    async loadTokenizer(source, options) {
      calls.tokenizerLoads.push({ source, options });
      return tokenizer;
    },
    async loadSafetensors(path, tensorName, dimension) {
      calls.tableLoads.push({ path, tensorName, dimension });
      return {
        data: Float32Array.from([1, 0, 0, 0, 1, 0, 0, 0, 2]),
        dimension: 3,
        dtype: "F32",
        rows: 3,
      };
    },
    async download(url, destination) {
      calls.downloads.push(url);
      await writeFile(
        destination,
        url.endsWith("tokenizer.json") ? "{}" : "weights",
      );
    },
  });
  t.after(() => setModel2VecRuntimeForTesting(null));

  const model = new Model2VecEmbeddingModel(entry(), {
    apiKey: "",
    modelCacheDir: root,
  });
  const vectors = await model.embed(
    [
      { kind: "text", text: "both tokens" },
      { kind: "text", text: "unknown-only" },
      { kind: "text", text: "third token" },
    ],
    { purpose: "query" },
  );

  assert.ok(Math.abs(vectors[0][0] - Math.SQRT1_2) < 1e-7);
  assert.ok(Math.abs(vectors[0][1] - Math.SQRT1_2) < 1e-7);
  assert.deepEqual(vectors[0].slice(2), [0]);
  assert.deepEqual(vectors[1], [0, 0, 0]);
  assert.deepEqual(vectors[2], [0, 0, 1]);
  assert.deepEqual(
    calls.tokenizerCalls.map(({ text }) => text),
    ["query: both tokens", "query: unknown-only", "query: third token"],
  );
  assert.deepEqual(calls.tokenizerCalls[0].options, {
    add_special_tokens: false,
    truncation: true,
    max_length: 513,
  });
  assert.equal(calls.downloads.length, 2);
  assert.ok(calls.downloads.some((url) => url.endsWith("model.safetensors")));
  assert.ok(calls.downloads.some((url) => url.endsWith("tokenizer.json")));
  assert.equal(calls.tokenizerLoads[0].options.local_files_only, true);
  assert.match(calls.tokenizerLoads[0].source, /tokenizer$/);
  assert.deepEqual(calls.tableLoads[0], {
    path: join(
      root,
      "model2vec",
      "test--potion",
      "0123456789abcdef",
      "model.safetensors",
    ),
    tensorName: "embeddings",
    dimension: 3,
  });

  await model.embed([{ kind: "text", text: "cached" }]);
  assert.equal(calls.downloads.length, 2);
  assert.equal(calls.tokenizerLoads.length, 1);
  assert.equal(calls.tableLoads.length, 1);

  await model.dispose();
  await model.dispose();
  await assert.rejects(
    model.embed([{ kind: "text", text: "after dispose" }]),
    /disposed/,
  );
});

test("Model2Vec parses real F32 and F16 Safetensors embedding tables", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-model2vec-safetensors-");
  setModel2VecRuntimeForTesting({
    async loadTokenizer() {
      return Object.assign(
        async (text) => ({
          input_ids: {
            data: BigInt64Array.from([text.includes("second") ? 1n : 0n]),
          },
        }),
        { unk_token_id: 99 },
      );
    },
    async download() {
      throw new Error("cached test assets should not be downloaded");
    },
  });
  t.after(() => setModel2VecRuntimeForTesting(null));

  const fixtures = [
    {
      dtype: "F32",
      repo: "test/f32",
      values: [1, 2, 3, 4, 5, 6],
    },
    {
      dtype: "F16",
      repo: "test/f16",
      values: [0x3c00, 0x4000, 0x4200, 0x4400, 0x4500, 0x4600],
    },
  ];

  for (const fixture of fixtures) {
    const cacheDirectory = join(
      root,
      "model2vec",
      fixture.repo.replaceAll("/", "--"),
      "0123456789abcdef",
    );
    await writeSafetensors(
      join(cacheDirectory, "model.safetensors"),
      fixture.dtype,
      fixture.values,
      [2, 3],
    );
    await mkdir(join(cacheDirectory, "tokenizer"), { recursive: true });
    await writeFile(join(cacheDirectory, "tokenizer", "tokenizer.json"), "{}");

    const model = new Model2VecEmbeddingModel(
      entry({
        id: `local/${fixture.dtype.toLowerCase()}`,
        model: fixture.dtype.toLowerCase(),
        repo: fixture.repo,
        normalize: false,
      }),
      {
        apiKey: "",
        modelCacheDir: root,
      },
    );
    assert.deepEqual(
      await model.embed([
        { kind: "text", text: "first" },
        { kind: "text", text: "second" },
      ]),
      [
        [1, 2, 3],
        [4, 5, 6],
      ],
    );
    await model.dispose();
  }
});

test("Model2Vec reports and truncates inputs beyond the model token limit", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-model2vec-truncate-");
  const calls = [];
  setModel2VecRuntimeForTesting({
    async loadTokenizer() {
      return Object.assign(
        async (_text, options) => {
          calls.push(options);
          return {
            input_ids: { data: BigInt64Array.from([0n, 1n, 2n]) },
          };
        },
        { unk_token_id: 99 },
      );
    },
    async loadSafetensors() {
      return {
        data: Float32Array.from([1, 0, 0, 0, 1, 0, 0, 0, 10]),
        dimension: 3,
        dtype: "F32",
        rows: 3,
      };
    },
    async download(_url, destination) {
      await writeFile(destination, "asset");
    },
  });
  t.after(() => setModel2VecRuntimeForTesting(null));

  const model = new Model2VecEmbeddingModel(entry({ maxInputTokens: 2 }), {
    apiKey: "",
    modelCacheDir: root,
  });
  const result = await model.embedWithDiagnostics([
    { kind: "text", text: "too many tokens" },
  ]);

  assert.deepEqual(result.diagnostics.truncatedInputIndexes, [0]);
  assert.ok(Math.abs(result.vectors[0][0] - Math.SQRT1_2) < 1e-7);
  assert.ok(Math.abs(result.vectors[0][1] - Math.SQRT1_2) < 1e-7);
  assert.equal(result.vectors[0][2], 0);
  assert.deepEqual(calls, [
    {
      add_special_tokens: false,
      truncation: true,
      max_length: 3,
    },
  ]);
});

test("Model2Vec rejects token ids outside the static embedding table", async (t) => {
  const root = await createTemporaryDirectory(t, "zvec-model2vec-invalid-");
  setModel2VecRuntimeForTesting({
    async loadTokenizer() {
      return Object.assign(
        async () => ({ input_ids: { data: BigInt64Array.from([3n]) } }),
        { unk_token_id: 99 },
      );
    },
    async loadSafetensors() {
      return {
        data: Float32Array.from([1, 0, 0, 0, 1, 0, 0, 0, 1]),
        dimension: 3,
        dtype: "F32",
        rows: 3,
      };
    },
    async download(_url, destination) {
      await writeFile(destination, "asset");
    },
  });
  t.after(() => setModel2VecRuntimeForTesting(null));

  const model = new Model2VecEmbeddingModel(entry(), {
    apiKey: "",
    modelCacheDir: root,
  });
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    (error) =>
      error.message === "Model2Vec embedding failed" &&
      error.cause?.message.includes("out-of-range token id"),
  );
});
