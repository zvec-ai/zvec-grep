import assert from "node:assert/strict";
import test from "node:test";
import { LlamaCppEmbeddingModel } from "../../../dist/engine/models/backends/llama-cpp.js";
import { Model2VecEmbeddingModel } from "../../../dist/engine/models/backends/model2vec.js";
import {
  Qwen37TextEmbeddingModel,
  Qwen3VlEmbeddingModel,
  QwenTextEmbeddingV4Model,
} from "../../../dist/engine/models/backends/qwen.js";
import { TransformersJsEmbeddingModel } from "../../../dist/engine/models/backends/transformers-js.js";
import { listEmbeddingModels } from "../../../dist/engine/models/catalog.js";
import { createEmbeddingModel } from "../../../dist/engine/models/factory.js";

test("embedding factory resolves catalog entries and rejects unknown models", () => {
  const options = { apiKey: "secret", endpoint: "https://example.test" };
  const expectedModelClasses = new Map([
    ["local/embeddinggemma-300m", LlamaCppEmbeddingModel],
    ["local/qwen3-embedding-0.6b", LlamaCppEmbeddingModel],
    ["qwen/text-embedding-v4", QwenTextEmbeddingV4Model],
    ["qwen/qwen3.7-text-embedding", Qwen37TextEmbeddingModel],
    ["qwen/qwen3-vl-embedding", Qwen3VlEmbeddingModel],
    ["local/bge-small-en-v1.5", TransformersJsEmbeddingModel],
    ["local/all-minilm-l6-v2", TransformersJsEmbeddingModel],
    ["local/potion-base-8m", Model2VecEmbeddingModel],
    ["local/potion-code-16m-v2", Model2VecEmbeddingModel],
    ["local/multilingual-e5-small", TransformersJsEmbeddingModel],
    ["local/jina-embeddings-v2-base-code", TransformersJsEmbeddingModel],
    ["local/gte-modernbert-base", TransformersJsEmbeddingModel],
    ["local/nomic-embed-text-v1.5", TransformersJsEmbeddingModel],
  ]);
  const catalogEntries = listEmbeddingModels();

  assert.deepEqual(
    catalogEntries.map((entry) => entry.reference).sort(),
    [...expectedModelClasses.keys()].sort(),
  );

  for (const entry of catalogEntries) {
    const model = createEmbeddingModel(entry.reference, options);
    const ExpectedModel = expectedModelClasses.get(entry.reference);

    assert.ok(
      model instanceof ExpectedModel,
      `${entry.reference} resolved to ${model.constructor.name}`,
    );
    assert.equal(model.info.reference, entry.reference);
    assert.equal(model.info.dimension, entry.dimension);
    assert.equal(model.info.metric, entry.metric);
    assert.equal(
      model.info.endpoint,
      entry.backend === "qwen" ? options.endpoint : undefined,
    );
  }

  assert.equal(
    createEmbeddingModel("qwen/text-embedding-v4", {
      apiKey: "secret",
    }).info.endpoint,
    "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings",
  );

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
