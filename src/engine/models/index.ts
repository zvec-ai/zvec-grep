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
  listEmbeddingModels,
} from "./catalog.js";
export {
  getRemoteEmbeddingProviderCatalogEntry,
  isRemoteEmbeddingProvider,
  listRemoteEmbeddingProviders,
} from "../remote-embedding-providers.js";
export {
  resolveEmbeddingReference,
  type ResolveEmbeddingReferenceOptions,
} from "./resolution.js";
