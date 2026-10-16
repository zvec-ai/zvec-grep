import { FakeEmbeddingModel } from "./fake-embedding.mjs";

export class CountingEmbeddingModel extends FakeEmbeddingModel {
  counts = { document: 0, query: 0 };
  async doEmbed(contents, options) {
    this.counts[options?.purpose ?? "document"] += contents.length;
    return super.doEmbed(contents, options);
  }
}
