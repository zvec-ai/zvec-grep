import assert from "node:assert/strict";
import test from "node:test";
import { EmbeddingModel } from "../../dist/engine/models/embeddings.js";
import {
  createEmbeddingModel,
  createEmbeddingModelFromCatalog,
  createEmbeddingModelFromReference,
} from "../../dist/engine/models/factory.js";
import {
  Qwen3VlEmbeddingModel,
  QwenTextEmbeddingV4Model,
} from "../../dist/engine/models/providers/qwen/embedding.js";

const vector = (dimension, value = 0.25) => Array(dimension).fill(value);

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

class StubEmbeddingModel extends EmbeddingModel {
  ref = { provider: "test", model: "stub" };
  dimension = 2;
  metric = "cosine";
  supportedContentKinds = ["text", "image"];
  limits = { maxBatchSize: 2, maxImageBytes: 3 };
  result = [[1, 0]];
  seenPurpose;

  async doEmbed(_contents, options) {
    this.seenPurpose = options.purpose;
    return this.result;
  }
}

test("embedding base class validates inputs, provider outputs, and purpose defaults", async () => {
  const model = new StubEmbeddingModel();
  assert.deepEqual(await model.embed([{ kind: "text", text: "hello" }]), [
    [1, 0],
  ]);
  assert.equal(model.seenPurpose, "document");
  await model.embed([{ kind: "text", text: "query" }], { purpose: "query" });
  assert.equal(model.seenPurpose, "query");
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

  model.supportedContentKinds = ["text"];
  await assert.rejects(
    model.embed([{ kind: "image", data: new Uint8Array([1]), format: "png" }]),
    /does not support/,
  );
  model.supportedContentKinds = ["text", "image"];

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
    () => new QwenTextEmbeddingV4Model({ apiKey: " " }),
    /requires an API key/,
  );
  assert.throws(
    () => new QwenTextEmbeddingV4Model({ apiKey: "secret", endpoint: "  " }),
    /requires an endpoint/,
  );
  const model = new QwenTextEmbeddingV4Model({
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
  assert.equal(result[0][0], 1);
  assert.equal(result[1][0], 2);
  assert.equal(request.headers.Authorization, "Bearer secret-value");
  assert.deepEqual(JSON.parse(request.body).input, ["first", "second"]);

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

test("Qwen VL model validates images, encodes bytes, and accepts provider index variants", async () => {
  assert.throws(
    () => new Qwen3VlEmbeddingModel({ apiKey: "" }),
    /requires an API key/,
  );
  assert.throws(
    () => new Qwen3VlEmbeddingModel({ apiKey: "secret", endpoint: " " }),
    /requires an endpoint/,
  );
  const model = new Qwen3VlEmbeddingModel({
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
  const result = await withFetch(
    async (_url, init) => {
      body = JSON.parse(init.body);
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
    result.map((item) => item[0]),
    [1, 2, 3, 4],
  );
  assert.equal(body.input.contents[1].image, "AQ==");
  assert.equal(body.input.contents[2].image, "AQI=");
  assert.equal(body.input.contents[3].image, "AQID");

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

test("embedding factory resolves catalog and explicit references and rejects unknown models", () => {
  const options = { apiKey: "secret", endpoint: "https://example.test" };
  assert.ok(
    createEmbeddingModel(
      { provider: "qwen", model: "text-embedding-v4" },
      options,
    ) instanceof QwenTextEmbeddingV4Model,
  );
  assert.ok(
    createEmbeddingModelFromReference(
      "qwen/qwen3-vl-embedding",
      options,
    ) instanceof Qwen3VlEmbeddingModel,
  );
  assert.equal(
    createEmbeddingModelFromCatalog("local/embeddinggemma-300m", options).ref
      .model,
    "embeddinggemma-300m",
  );
  assert.equal(
    createEmbeddingModelFromReference("local/qwen3-embedding-0.6b", options).ref
      .model,
    "qwen3-embedding-0.6b",
  );
  assert.throws(
    () => createEmbeddingModelFromCatalog("missing", options),
    /not in the zvec-grep catalog/,
  );
  assert.throws(
    () => createEmbeddingModelFromReference("invalid", options),
    /not in the zvec-grep catalog/,
  );
  assert.throws(
    () =>
      createEmbeddingModel({ provider: "unknown", model: "missing" }, options),
    /not implemented/,
  );
  assert.throws(
    () =>
      createEmbeddingModel({ provider: "local", model: "missing" }, options),
    /not in the zvec-grep catalog/,
  );
});
