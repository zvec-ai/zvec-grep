export {
  EMBEDDING_MODEL_CATALOG,
  MODEL_CATALOG,
  getEmbeddingModelCatalogEntry,
  getEmbeddingModelCatalogEntryByRef,
  listEmbeddingModels,
} from "./catalog.js";

export { EmbeddingModel } from "./embeddings.js";

export {
  LlamaCppEmbeddingModel,
  Qwen3VlEmbeddingModel,
  QwenTextEmbeddingV4Model,
  setLlamaCppRuntimeForTesting,
} from "./providers/index.js";

export type {
  RankingCandidate,
  RankingModel,
  RankingScore,
} from "./ranking.js";

export {
  createEmbeddingModel,
  createEmbeddingModelFromCatalog,
  createEmbeddingModelFromReference,
} from "./factory.js";

export type {
  EmbeddingCatalogEntry,
  LlamaCppEmbeddingCatalogEntry,
  LlamaGpuMode,
  LocalEmbeddingFormat,
  ModelProviderOptions,
} from "./types.js";
