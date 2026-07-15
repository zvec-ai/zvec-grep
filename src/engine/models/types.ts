export type ModelProviderOptions = {
  apiKey: string;
  endpoint?: string;
  modelCacheDir?: string;
  llamaGpu?: LlamaGpuMode;
  embeddingParallelism?: number;
};

export type ModelCatalog = Record<
  string,
  {
    embedding: readonly string[];
    ranking: readonly string[];
  }
>;

export type ModelRef = {
  provider: string;
  model: string;
};

export type VectorMetric = "cosine" | "dot" | "euclidean";

export type LlamaGpuMode = "auto" | "metal" | "vulkan" | "cuda" | false;

export type LocalEmbeddingFormat = "embeddinggemma" | "qwen3";

export type LlamaCppEmbeddingCatalogEntry = {
  id: string;
  provider: "local";
  model: string;
  uri: string;
  dimension: number;
  metric: VectorMetric;
  format: LocalEmbeddingFormat;
  contextSize: number;
  maxBatchSize: number;
};

export type EmbeddingCatalogEntry = LlamaCppEmbeddingCatalogEntry;
