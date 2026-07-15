import type { EmbeddingCatalogEntry, ModelCatalog, ModelRef } from "./types.js";

export const MODEL_CATALOG = {
  openai: {
    embedding: ["text-embedding-3-small", "text-embedding-3-large"],
    ranking: [],
  },

  qwen: {
    embedding: ["text-embedding-v4", "qwen3-vl-embedding"],
    ranking: ["gte-rerank-v2"],
  },

  jina: {
    embedding: ["jina-embeddings-v3"],
    ranking: ["jina-reranker-v2-base-multilingual"],
  },

  local: {
    embedding: ["embeddinggemma-300m", "qwen3-embedding-0.6b"],
    ranking: [],
  },
} as const satisfies ModelCatalog;

export const EMBEDDING_MODEL_CATALOG = {
  "local/embeddinggemma-300m": {
    id: "local/embeddinggemma-300m",
    provider: "local",
    model: "embeddinggemma-300m",
    uri: "hf:ggml-org/embeddinggemma-300M-GGUF/embeddinggemma-300M-Q8_0.gguf",
    dimension: 768,
    metric: "cosine",
    format: "embeddinggemma",
    contextSize: 2048,
    maxBatchSize: 16,
  },

  "local/qwen3-embedding-0.6b": {
    id: "local/qwen3-embedding-0.6b",
    provider: "local",
    model: "qwen3-embedding-0.6b",
    uri: "hf:Qwen/Qwen3-Embedding-0.6B-GGUF/Qwen3-Embedding-0.6B-Q8_0.gguf",
    dimension: 1024,
    metric: "cosine",
    format: "qwen3",
    contextSize: 8192,
    maxBatchSize: 8,
  },
} as const satisfies Record<string, EmbeddingCatalogEntry>;

export type EmbeddingModelCatalogId = keyof typeof EMBEDDING_MODEL_CATALOG;

export function listEmbeddingModels(): EmbeddingCatalogEntry[] {
  return Object.values(EMBEDDING_MODEL_CATALOG);
}

export function getEmbeddingModelCatalogEntry(
  id: string,
): EmbeddingCatalogEntry | undefined {
  return EMBEDDING_MODEL_CATALOG[id as EmbeddingModelCatalogId];
}

export function getEmbeddingModelCatalogEntryByRef(
  ref: ModelRef,
): EmbeddingCatalogEntry | undefined {
  return listEmbeddingModels().find(
    (entry) => entry.provider === ref.provider && entry.model === ref.model,
  );
}
