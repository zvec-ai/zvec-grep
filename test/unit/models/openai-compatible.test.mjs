import assert from "node:assert/strict";
import test from "node:test";
import { OpenAiCompatibleTextEmbeddingModel } from "../../../dist/engine/models/backends/openai-compatible.js";

const entry = {
  reference: "dgx/qwen3-embedding-0.6b",
  provider: "dgx",
  model: "qwen3-embedding:0.6b",
  dimension: 3,
  metric: "cosine",
  maxBatchSize: 32,
  maxBatchChars: 64000,
  maxInputTokens: 8192,
};

const vector = (value = 0.25) => Array(entry.dimension).fill(value);

function jsonResponse(body, init = {}) {
  return new Response(JSON.stringify(body), {
    status: init.status ?? 200,
    headers: { "content-type": "application/json", ...init.headers },
  });
}

test("generic OpenAI-compatible model sends the minimal DGX request and restores response order", async () => {
  let request;
  const model = new OpenAiCompatibleTextEmbeddingModel(
    entry,
    { endpoint: " http://spark:11434/v1/embeddings " },
    undefined,
    {
      async fetch(url, init) {
        request = { url, init, body: JSON.parse(init.body) };
        if ("dimensions" in request.body || "encoding_format" in request.body) {
          return jsonResponse(
            { error: { message: "unsupported optional field" } },
            { status: 400 },
          );
        }
        return jsonResponse({
          data: [
            { index: 1, embedding: vector(2) },
            { index: 0, embedding: vector(1) },
          ],
        });
      },
    },
  );

  const result = await model.embed([
    { kind: "text", text: "first" },
    { kind: "text", text: "second" },
  ]);

  assert.equal(request.url, "http://spark:11434/v1/embeddings");
  assert.equal(model.info.limits.maxBatchChars, 64000);
  assert.deepEqual(request.body, {
    model: "qwen3-embedding:0.6b",
    input: ["first", "second"],
  });
  assert.equal(request.init.headers.Authorization, undefined);
  assert.equal(request.init.headers["Content-Type"], "application/json");
  assert.deepEqual(result.vectors, [vector(1), vector(2)]);
});

test("generic OpenAI-compatible model sends declared optional request fields and credentials", async () => {
  let request;
  const model = new OpenAiCompatibleTextEmbeddingModel(
    {
      ...entry,
      requestDimensions: true,
      requestEncodingFormat: true,
    },
    { endpoint: "https://example.test/embeddings", apiKey: "secret" },
    undefined,
    {
      async fetch(_url, init) {
        request = init;
        return jsonResponse({ data: [{ index: 0, embedding: vector() }] });
      },
    },
  );

  await model.embed([{ kind: "text", text: "value" }]);

  assert.equal(request.headers.Authorization, "Bearer secret");
  assert.deepEqual(JSON.parse(request.body), {
    model: "qwen3-embedding:0.6b",
    input: ["value"],
    dimensions: 3,
    encoding_format: "float",
  });
});

test("generic OpenAI-compatible model validates endpoint and indexed response shape", async () => {
  assert.throws(
    () => new OpenAiCompatibleTextEmbeddingModel(entry, {}),
    /requires an endpoint/,
  );

  async function rejectsResponse(body, expected) {
    const model = new OpenAiCompatibleTextEmbeddingModel(
      entry,
      { endpoint: "https://example.test/embeddings" },
      undefined,
      { fetch: async () => jsonResponse(body) },
    );
    await assert.rejects(
      model.embed([
        { kind: "text", text: "first" },
        { kind: "text", text: "second" },
      ]),
      expected,
    );
  }

  await rejectsResponse(
    {
      data: [
        { index: 0, embedding: vector() },
        { index: 0, embedding: vector() },
      ],
    },
    /duplicate index/,
  );
  await rejectsResponse(
    { data: [{ index: 0, embedding: vector() }] },
    /non-array vector/,
  );
  await rejectsResponse(
    {
      data: [
        { index: 0, embedding: [1, 2] },
        { index: 1, embedding: vector() },
      ],
    },
    /wrong dimension/,
  );
  await rejectsResponse(
    {
      data: [
        { index: 0, embedding: [1, Number.NaN, 3] },
        { index: 1, embedding: vector() },
      ],
    },
    /non-finite vector value/,
  );
});

test("DGX allows slow inference while hosted models keep their timeout", async (t) => {
  const timeouts = [];
  t.mock.method(AbortSignal, "timeout", (ms) => {
    timeouts.push(ms);
    return new AbortController().signal;
  });
  for (const provider of ["dgx", "qwen"]) {
    const model = new OpenAiCompatibleTextEmbeddingModel(
      { ...entry, provider },
      { endpoint: "http://example.test/embeddings" },
      undefined,
      {
        async fetch() {
          return jsonResponse({ data: [{ index: 0, embedding: vector() }] });
        },
      },
    );
    assert.equal(
      model.info.defaultConcurrency,
      provider === "dgx" ? 1 : undefined,
    );
    await model.embed([{ kind: "text", text: "value" }]);
  }
  assert.deepEqual(timeouts, [1_800_000, 60_000]);
});
