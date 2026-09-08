export {
  EmbeddingPurpose,
  type CreateEmbeddingModelOptions,
  type EmbeddingModel,
  type EmbeddingModelProgress,
  type EmbeddingModelInfo,
  type EmbeddingOptions,
  type EmbeddingResult,
} from "./embeddings.js";
export { createEmbeddingModel } from "./factory.js";
export {
  getEmbeddingModelCatalogEntry,
  getRemoteEmbeddingProviderCatalogEntry,
  listEmbeddingModels,
  listRemoteEmbeddingProviders,
} from "./catalog.js";
export {
  resolveEmbeddingReference,
  type ResolveEmbeddingReferenceOptions,
} from "./resolution.js";
