import { EngineError } from "../errors/index.js";
import { getEmbeddingModelCatalogEntry } from "./catalog.js";
import type {
  CreateEmbeddingModelOptions,
  EmbeddingModel,
} from "./embeddings.js";
import { LlamaCppEmbeddingModel } from "./backends/llama-cpp.js";
import { Model2VecEmbeddingModel } from "./backends/model2vec.js";
import {
  Qwen37TextEmbeddingModel,
  Qwen3VlEmbeddingModel,
  QwenTextEmbeddingV4Model,
} from "./backends/qwen.js";
import { TransformersJsEmbeddingModel } from "./backends/transformers-js.js";
export function createEmbeddingModel(
  reference: string,
  options: CreateEmbeddingModelOptions = {},
): EmbeddingModel {
  const catalogEntry = getEmbeddingModelCatalogEntry(reference);
  if (!catalogEntry) {
    throw new EngineError("Embedding model is not in the zvec-grep catalog", {
      code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_CATALOG_MODEL_NOT_FOUND",
      context: `embedding=${reference}`,
    });
  }

  if (catalogEntry.backend === "llama-cpp") {
    return new LlamaCppEmbeddingModel(catalogEntry, options);
  }

  if (catalogEntry.backend === "model2vec") {
    return new Model2VecEmbeddingModel(catalogEntry, options);
  }

  if (catalogEntry.backend === "transformers-js") {
    return new TransformersJsEmbeddingModel(catalogEntry, options);
  }

  if (catalogEntry.kind === "multimodal") {
    return new Qwen3VlEmbeddingModel(catalogEntry, options);
  }

  if (catalogEntry.model === "text-embedding-v4") {
    return new QwenTextEmbeddingV4Model(catalogEntry, options);
  }

  return new Qwen37TextEmbeddingModel(catalogEntry, options);
}
