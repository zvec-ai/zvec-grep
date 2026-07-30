import assert from "node:assert/strict";
import test from "node:test";
import { BaseEmbeddingModel } from "../../dist/engine/models/embeddings.js";
import { getEmbeddingModelCatalogEntry } from "../../dist/engine/models/catalog.js";
import { createEmbeddingModel } from "../../dist/engine/models/factory.js";
import {
  Qwen37TextEmbeddingModel,
  Qwen3VlEmbeddingModel,
  QwenTextEmbeddingV4Model,
} from "../../dist/engine/models/backends/qwen.js";
import { TransformersJsEmbeddingModel } from "../../dist/engine/models/backends/transformers-js.js";

const vector = (dimension, value = 0.25) => Array(dimension).fill(value);
const qwenTextEntry = getEmbeddingModelCatalogEntry("qwen/text-embedding-v4");
const qwen37TextEntry = getEmbeddingModelCatalogEntry(
  "qwen/qwen3.7-text-embedding",
);
const qwenVlEntry = getEmbeddingModelCatalogEntry("qwen/qwen3-vl-embedding");

async function withFetch(mock, callback) {
  const original = globalThis.fetch;
  globalThis.fetch = mock;
  try {
    return await callback();
  } finally {
    globalThis.fetch = original;
  }
}

function jsonResponse(body, init = {}) {
  return new Response(JSON.stringify(body), {
    status: init.status ?? 200,
    headers: { "content-type": "application/json", ...init.headers },
  });
}

class StubEmbeddingModel extends BaseEmbeddingModel {
  info = {
    reference: "test/stub",
    provider: "test",
    name: "stub",
    dimension: 2,
    metric: "cosine",
    inputKinds: ["text", "image"],
    limits: { maxBatchSize: 2, maxImageBytes: 3 },
  };
  result = [[1, 0]];
  seenPurpose;
  seenSignal;

  async doEmbed(_contents, options) {
    this.seenPurpose = options.purpose;
    this.seenSignal = options.signal;
    return { vectors: this.result, truncated: [] };
  }
}

test("embedding base class validates inputs, provider outputs, and purpose defaults", async () => {
  const model = new StubEmbeddingModel();
  assert.deepEqual(await model.embed([{ kind: "text", text: "hello" }]), {
    vectors: [[1, 0]],
    truncated: [],
  });
  assert.equal(model.seenPurpose, "document");
  const signal = new AbortController().signal;
  await model.embed([{ kind: "text", text: "query" }], {
    purpose: "query",
    signal,
  });
  assert.equal(model.seenPurpose, "query");
  assert.equal(model.seenSignal, signal);
  await model.dispose();

  await assert.rejects(model.embed([]), /at least one/);
  await assert.rejects(
    model.embed([
      { kind: "text", text: "one" },
      { kind: "text", text: "two" },
      { kind: "text", text: "three" },
    ]),
    /batch size/,
  );
  await assert.rejects(
    model.embed([{ kind: "text", text: "  " }]),
    /must not be empty/,
  );
  await assert.rejects(
    model.embed([{ kind: "image", data: new Uint8Array(), format: "png" }]),
    /must not be empty/,
  );
  await assert.rejects(
    model.embed([{ kind: "image", data: new Uint8Array([1]), format: "" }]),
    /include a format/,
  );
  await assert.rejects(
    model.embed([
      { kind: "image", data: new Uint8Array([1, 2, 3, 4]), format: "png" },
    ]),
    /exceeds model limit/,
  );

  model.info.inputKinds = ["text"];
  await assert.rejects(
    model.embed([{ kind: "image", data: new Uint8Array([1]), format: "png" }]),
    /does not support/,
  );
  model.info.inputKinds = ["text", "image"];

  model.result = null;
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    /non-array response/,
  );
  model.result = [];
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    /wrong number/,
  );
  model.result = [null];
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    /non-array vector/,
  );
  model.result = [[1]];
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    /wrong dimension/,
  );
  model.result = [[1, Number.NaN]];
  await assert.rejects(
    model.embed([{ kind: "text", text: "value" }]),
    /non-finite/,
  );
});

test("Qwen text model sends ordered batches and validates all response shapes", async () => {
  assert.throws(
    () => new QwenTextEmbeddingV4Model(qwenTextEntry, { apiKey: " " }),
    /requires an API key/,
  );
  assert.throws(
    () =>
      new QwenTextEmbeddingV4Model(qwenTextEntry, {
        apiKey: "secret",
        endpoint: "  ",
      }),
    /requires an endpoint/,
  );
  const model = new QwenTextEmbeddingV4Model(qwenTextEntry, {
    apiKey: "secret-value",
    endpoint: " https://example.test/embeddings ",
  });
  let request;
  const result = await withFetch(
    async (_url, init) => {
      request = init;
      return jsonResponse({
        data: [
          { index: 1, embedding: vector(1024, 2) },
          { index: 0, embedding: vector(1024, 1) },
        ],
      });
    },
    () =>
      model.embed([
        { kind: "text", text: "first" },
        { kind: "text", text: "second" },
      ]),
  );
  assert.equal(result.vectors[0][0], 1);
  assert.equal(result.vectors[1][0], 2);
  assert.equal(request.headers.Authorization, "Bearer secret-value");
  assert.ok(request.signal instanceof AbortSignal);
  assert.deepEqual(JSON.parse(request.body).input, ["first", "second"]);

  await withFetch(
    async (_url, init) =>
      new Promise((resolve, reject) => {
        if (init.signal.aborted) {
          reject(init.signal.reason);
          return;
        }
        init.signal.addEventListener(
          "abort",
          () => reject(init.signal.reason),
          { once: true },
        );
      }),
    async () => {
      const controller = new AbortController();
      const cancelled = assert.rejects(
        model.embed([{ kind: "text", text: "cancel" }], {
          signal: controller.signal,
        }),
        /stop embedding/,
      );
      controller.abort(new Error("stop embedding"));
      await cancelled;
    },
  );

  await withFetch(
    async () => {
      throw new Error("offline");
    },
    () =>
      assert.rejects(
        model.embed([{ kind: "text", text: "value" }]),
        /request failed/,
      ),
  );
  await withFetch(
    async () => new Response("not-json", { status: 200 }),
    () =>
      assert.rejects(
        model.embed([{ kind: "text", text: "value" }]),
        /not valid JSON/,
      ),
  );
  await withFetch(
    async () =>
      jsonResponse(
        { error: { code: "Throttled", type: "rate", message: "slow" } },
        { status: 429, headers: { "retry-after": "1.5" } },
      ),
    () =>
      assert.rejects(
        model.embed([{ kind: "text", text: "value" }]),
        (error) =>
          /returned an error/.test(error.message) &&
          /retryAfterMs=1500/.test(error.context) &&
          !error.context.includes("secret-value"),
      ),
  );
  for (const [errorBody, retryAfter] of [
    [null, undefined],
    [{ code: "FlatError", message: "flat provider error" }, "invalid-date"],
    [{ error: {} }, new Date(Date.now() + 60_000).toUTCString()],
  ]) {
    await withFetch(
      async () =>
        jsonResponse(errorBody, {
          status: 500,
          headers: retryAfter ? { "retry-after": retryAfter } : {},
        }),
      () =>
        assert.rejects(
          model.embed([{ kind: "text", text: "value" }]),
          /returned an error/,
        ),
    );
  }

  for (const [body, message] of [
    [{}, /did not include data/],
    [{ data: [null] }, /invalid index/],
    [{ data: [{ index: 1, embedding: [] }] }, /out of range/],
    [{ data: [{ index: 0 }] }, /invalid embedding/],
    [{ data: [] }, /non-array vector/],
  ]) {
    await withFetch(
      async () => jsonResponse(body),
      () =>
        assert.rejects(model.embed([{ kind: "text", text: "value" }]), message),
    );
  }
});

test("Qwen3.7 text embedding uses its model name and expanded limits", async () => {
  assert.throws(
    () => new Qwen37TextEmbeddingModel(qwen37TextEntry, { apiKey: " " }),
    /requires an API key/,
  );

  const model = new Qwen37TextEmbeddingModel(qwen37TextEntry, {
    apiKey: "secret-value",
    endpoint: "https://example.test/embeddings",
  });
  assert.deepEqual(model.info.limits, {
    maxBatchSize: 20,
    maxInputTokens: 128000,
  });

  let body;
  let requestSignal;
  const result = await withFetch(
    async (_url, init) => {
      body = JSON.parse(init.body);
      requestSignal = init.signal;
      return jsonResponse({
        data: [{ index: 0, embedding: vector(1024, 3) }],
      });
    },
    () => model.embed([{ kind: "text", text: "find relevant code" }]),
  );

  assert.equal(result.vectors[0][0], 3);
  assert.equal(body.model, "qwen3.7-text-embedding");
  assert.equal(body.dimensions, 1024);
  assert.equal(body.encoding_format, "float");
  assert.ok(requestSignal instanceof AbortSignal);

  await assert.rejects(
    model.embed(
      Array.from({ length: 21 }, (_, index) => ({
        kind: "text",
        text: `input-${index}`,
      })),
    ),
    /batch size exceeds model limit/,
  );
});

test("Qwen VL model validates images, encodes bytes, and accepts provider index variants", async () => {
  assert.throws(
    () => new Qwen3VlEmbeddingModel(qwenVlEntry, { apiKey: "" }),
    /requires an API key/,
  );
  assert.throws(
    () =>
      new Qwen3VlEmbeddingModel(qwenVlEntry, {
        apiKey: "secret",
        endpoint: " ",
      }),
    /requires an endpoint/,
  );
  const model = new Qwen3VlEmbeddingModel(qwenVlEntry, {
    apiKey: "secret",
    endpoint: "https://example.test/vl",
  });
  await assert.rejects(
    model.embed([{ kind: "image", data: new Uint8Array([1]), format: "gif" }]),
    /does not support image format/,
  );
  await assert.rejects(
    model.embed(
      Array.from({ length: 11 }, () => ({
        kind: "image",
        data: new Uint8Array([1]),
        format: "png",
      })),
    ),
    /image count exceeds/,
  );

  let body;
  let vlRequestSignal;
  const result = await withFetch(
    async (_url, init) => {
      body = JSON.parse(init.body);
      vlRequestSignal = init.signal;
      return jsonResponse({
        output: {
          embeddings: [
            { text_index: 1, embedding: vector(2560, 2) },
            { index: 0, embedding: vector(2560, 1) },
            { embedding: vector(2560, 3) },
            { embedding: vector(2560, 4) },
          ],
        },
      });
    },
    () =>
      model.embed([
        { kind: "text", text: "text" },
        { kind: "image", data: new Uint8Array([1]), format: "png" },
        { kind: "image", data: new Uint8Array([1, 2]), format: "jpeg" },
        { kind: "image", data: new Uint8Array([1, 2, 3]), format: "webp" },
      ]),
  );
  assert.deepEqual(
    result.vectors.map((item) => item[0]),
    [1, 2, 3, 4],
  );
  assert.equal(body.input.contents[1].image, "AQ==");
  assert.equal(body.input.contents[2].image, "AQI=");
  assert.equal(body.input.contents[3].image, "AQID");
  assert.ok(vlRequestSignal instanceof AbortSignal);

  await withFetch(
    async () => {
      throw new Error("offline");
    },
    () =>
      assert.rejects(
        model.embed([{ kind: "text", text: "value" }]),
        /request failed/,
      ),
  );
  await withFetch(
    async () => new Response("invalid"),
    () =>
      assert.rejects(
        model.embed([{ kind: "text", text: "value" }]),
        /not valid JSON/,
      ),
  );
  await withFetch(
    async () =>
      jsonResponse(
        { code: "BadRequest", message: "bad input" },
        { status: 400, headers: { "retry-after": "not-a-date" } },
      ),
    () =>
      assert.rejects(
        model.embed([{ kind: "text", text: "value" }]),
        /returned an error/,
      ),
  );
  for (const [responseBody, message] of [
    [{ output: {} }, /did not include embeddings/],
    [{ output: { embeddings: [null] } }, /invalid embedding item/],
    [{ output: { embeddings: [{ index: 2, embedding: [] }] } }, /out of range/],
    [{ output: { embeddings: [{ index: 0 }] } }, /invalid embedding/],
  ]) {
    await withFetch(
      async () => jsonResponse(responseBody),
      () =>
        assert.rejects(model.embed([{ kind: "text", text: "value" }]), message),
    );
  }
});

test("embedding factory resolves catalog entries and rejects unknown models", () => {
  const options = { apiKey: "secret", endpoint: "https://example.test" };
  assert.ok(
    createEmbeddingModel("qwen/text-embedding-v4", options) instanceof
      QwenTextEmbeddingV4Model,
  );
  assert.ok(
    createEmbeddingModel("qwen/qwen3.7-text-embedding", options) instanceof
      Qwen37TextEmbeddingModel,
  );
  assert.ok(
    createEmbeddingModel("qwen/qwen3-vl-embedding", options) instanceof
      Qwen3VlEmbeddingModel,
  );
  assert.equal(
    createEmbeddingModel("local/embeddinggemma-300m", options).info.reference,
    "local/embeddinggemma-300m",
  );
  assert.equal(
    createEmbeddingModel("local/qwen3-embedding-0.6b", options).info.reference,
    "local/qwen3-embedding-0.6b",
  );
  assert.ok(
    createEmbeddingModel("local/bge-small-en-v1.5", options) instanceof
      TransformersJsEmbeddingModel,
  );
  for (const reference of [
    "local/multilingual-e5-small",
    "local/jina-embeddings-v2-base-code",
    "local/gte-modernbert-base",
    "local/nomic-embed-text-v1.5",
  ]) {
    assert.ok(
      createEmbeddingModel(reference, options) instanceof
        TransformersJsEmbeddingModel,
    );
  }
  assert.throws(
    () => createEmbeddingModel("missing", options),
    /not in the zvec-grep catalog/,
  );
  assert.throws(
    () => createEmbeddingModel("invalid", options),
    /not in the zvec-grep catalog/,
  );
  assert.throws(
    () => createEmbeddingModel("unknown/missing", options),
    /not in the zvec-grep catalog/,
  );
  assert.throws(
    () => createEmbeddingModel("local/missing", options),
    /not in the zvec-grep catalog/,
  );
});
