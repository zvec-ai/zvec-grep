import assert from "node:assert/strict";
import { createServer } from "node:http";
import { BaseEmbeddingModel } from "../../dist/engine/models/embeddings.js";

export class FakeEmbeddingModel extends BaseEmbeddingModel {
  info = {
    reference: "test/deterministic",
    provider: "test",
    name: "deterministic",
    dimension: 16,
    metric: "cosine",
    inputKinds: ["text"],
    limits: { maxBatchSize: 128 },
  };

  async doEmbed(contents) {
    return {
      vectors: contents.map((content) =>
        deterministicVector(content.text, this.info.dimension),
      ),
      truncated: [],
    };
  }
}

export function deterministicVector(value, dimension = 1024) {
  const vector = new Array(dimension).fill(0);
  const normalized = String(value).toLowerCase();
  for (let index = 0; index < normalized.length; index++) {
    vector[normalized.charCodeAt(index) % dimension] += 1;
  }
  const magnitude = Math.sqrt(
    vector.reduce((sum, item) => sum + item * item, 0),
  );
  return vector.map((item, index) =>
    magnitude === 0 ? (index === 0 ? 1 : 0) : item / magnitude,
  );
}

export async function createFakeEmbeddingServer(t, dimension = 1024) {
  const server = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const body = JSON.parse(Buffer.concat(chunks).toString("utf8"));
    const inputs = Array.isArray(body.input) ? body.input : [];
    response.writeHead(200, { "content-type": "application/json" });
    response.end(
      JSON.stringify({
        data: inputs.map((input, index) => ({
          index,
          embedding: deterministicVector(input, dimension),
        })),
      }),
    );
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(
    () =>
      new Promise((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      ),
  );
  const address = server.address();
  assert.ok(address && typeof address === "object");
  return `http://127.0.0.1:${address.port}/embeddings`;
}
