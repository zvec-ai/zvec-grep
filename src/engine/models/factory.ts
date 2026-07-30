import { EngineError } from "../errors/index.js";
import {
  getEmbeddingModelCatalogEntry,
  type QwenEmbeddingCatalogEntry,
} from "./catalog.js";
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

  switch (catalogEntry.backend) {
    case "llama-cpp":
      return new LlamaCppEmbeddingModel(catalogEntry, options);
    case "model2vec":
      return new Model2VecEmbeddingModel(catalogEntry, options);
    case "transformers-js":
      return new TransformersJsEmbeddingModel(catalogEntry, options);
    case "qwen":
      return createQwenEmbeddingModel(catalogEntry, options);
    default:
      return unsupportedCatalogEntry(catalogEntry);
  }
}

function createQwenEmbeddingModel(
  entry: QwenEmbeddingCatalogEntry,
  options: CreateEmbeddingModelOptions,
): EmbeddingModel {
  switch (entry.kind) {
    case "multimodal":
      return new Qwen3VlEmbeddingModel(entry, options);
    case "text":
      switch (entry.model) {
        case "text-embedding-v4":
          return new QwenTextEmbeddingV4Model(entry, options);
        case "qwen3.7-text-embedding":
          return new Qwen37TextEmbeddingModel(entry, options);
        default:
          return unsupportedCatalogEntry(entry);
      }
    default:
      return unsupportedCatalogEntry(entry);
  }
}

function unsupportedCatalogEntry(entry: never): never {
  throw new EngineError("Embedding catalog entry is not implemented", {
    code: "ZVEC_GREP.ENGINE.MODELS.EMBEDDING_MODEL_NOT_IMPLEMENTED",
    context: JSON.stringify(entry),
  });
}
