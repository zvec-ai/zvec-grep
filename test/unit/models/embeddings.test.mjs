import assert from "node:assert/strict";
import test from "node:test";
import { BaseEmbeddingModel } from "../../../dist/engine/models/embeddings.js";

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
