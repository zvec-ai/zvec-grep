import assert from "node:assert/strict";
import test from "node:test";
import { getEmbeddingModelCatalogEntry } from "../../../dist/engine/models/catalog.js";
import {
  Qwen37TextEmbeddingModel,
  Qwen3VlEmbeddingModel,
  QwenTextEmbeddingV4Model,
} from "../../../dist/engine/models/backends/qwen.js";

const vector = (dimension, value = 0.25) => Array(dimension).fill(value);
const qwenTextEntry = getEmbeddingModelCatalogEntry("qwen/text-embedding-v4");
const qwen37TextEntry = getEmbeddingModelCatalogEntry(
  "qwen/qwen3.7-text-embedding",
);
const qwenVlEntry = getEmbeddingModelCatalogEntry("qwen/qwen3-vl-embedding");

function createDependencies() {
  let currentFetch;
  return {
    dependencies: {
      fetch(...args) {
        if (!currentFetch) {
          throw new Error("Fetch dependency is not configured");
        }
        return currentFetch(...args);
      },
    },
    async withFetch(fetch, callback) {
      const previousFetch = currentFetch;
      currentFetch = fetch;
      try {
        return await callback();
      } finally {
        currentFetch = previousFetch;
      }
    },
  };
}

function jsonResponse(body, init = {}) {
  return new Response(JSON.stringify(body), {
    status: init.status ?? 200,
    headers: { "content-type": "application/json", ...init.headers },
  });
}

test("Qwen text model sends ordered batches and validates all response shapes", async () => {
  const { dependencies, withFetch } = createDependencies();
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
  const model = new QwenTextEmbeddingV4Model(
    qwenTextEntry,
    {
      apiKey: "secret-value",
      endpoint: " https://example.test/embeddings ",
    },
    dependencies,
  );
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
  const { dependencies, withFetch } = createDependencies();
  assert.throws(
    () => new Qwen37TextEmbeddingModel(qwen37TextEntry, { apiKey: " " }),
    /requires an API key/,
  );

  const model = new Qwen37TextEmbeddingModel(
    qwen37TextEntry,
    {
      apiKey: "secret-value",
      endpoint: "https://example.test/embeddings",
    },
    dependencies,
  );
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
  const { dependencies, withFetch } = createDependencies();
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
  const model = new Qwen3VlEmbeddingModel(
    qwenVlEntry,
    {
      apiKey: "secret",
      endpoint: "https://example.test/vl",
    },
    dependencies,
  );
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
