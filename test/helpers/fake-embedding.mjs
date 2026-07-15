import { EmbeddingModel } from "../../dist/engine/models/embeddings.js";

export class FakeEmbeddingModel extends EmbeddingModel {
  ref = { provider: "test", model: "deterministic" };
  dimension = 16;
  metric = "cosine";
  supportedContentKinds = ["text"];
  limits = { maxBatchSize: 128 };

  async doEmbed(contents) {
    return contents.map((content) =>
      deterministicVector(content.text, this.dimension),
    );
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
