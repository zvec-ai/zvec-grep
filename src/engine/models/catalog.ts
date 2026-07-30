export const EMBEDDING_MODEL_CATALOG = {
  "local/embeddinggemma-300m": {
    backend: "llama-cpp",
    reference: "local/embeddinggemma-300m",
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
    backend: "llama-cpp",
    reference: "local/qwen3-embedding-0.6b",
    provider: "local",
    model: "qwen3-embedding-0.6b",
    uri: "hf:Qwen/Qwen3-Embedding-0.6B-GGUF/Qwen3-Embedding-0.6B-Q8_0.gguf",
    dimension: 1024,
    metric: "cosine",
    format: "qwen3",
    contextSize: 8192,
    maxBatchSize: 8,
  },

  "qwen/text-embedding-v4": {
    backend: "qwen",
    kind: "text",
    reference: "qwen/text-embedding-v4",
    provider: "qwen",
    model: "text-embedding-v4",
    dimension: 1024,
    metric: "cosine",
    maxBatchSize: 10,
    maxInputTokens: 8192,
  },

  "qwen/qwen3.7-text-embedding": {
    backend: "qwen",
    kind: "text",
    reference: "qwen/qwen3.7-text-embedding",
    provider: "qwen",
    model: "qwen3.7-text-embedding",
    dimension: 1024,
    metric: "cosine",
    maxBatchSize: 20,
    maxInputTokens: 128000,
  },

  "qwen/qwen3-vl-embedding": {
    backend: "qwen",
    kind: "multimodal",
    reference: "qwen/qwen3-vl-embedding",
    provider: "qwen",
    model: "qwen3-vl-embedding",
    dimension: 2560,
    metric: "cosine",
    maxBatchSize: 20,
    maxInputTokens: 32000,
    maxImageBytes: 10 * 1024 * 1024,
  },

  "local/bge-small-en-v1.5": {
    backend: "transformers-js",
    reference: "local/bge-small-en-v1.5",
    provider: "local",
    model: "bge-small-en-v1.5",
    repo: "onnx-community/bge-small-en-v1.5-ONNX",
    revision: "4a9a46c7b88fa408e650a571a1800243f26309bd",
    dtype: "q4",
    dimension: 384,
    metric: "cosine",
    pooling: "cls",
    normalize: true,
    queryPrefix: "Represent this sentence for searching relevant passages: ",
    maxInputTokens: 512,
    maxBatchSize: 4,
  },

  "local/all-minilm-l6-v2": {
    backend: "transformers-js",
    reference: "local/all-minilm-l6-v2",
    provider: "local",
    model: "all-minilm-l6-v2",
    repo: "onnx-community/all-MiniLM-L6-v2-ONNX",
    revision: "aff7a1dc4e8a1ea593e6ea21e95c22ef0a25966f",
    dtype: "q4",
    dimension: 384,
    metric: "cosine",
    pooling: "mean",
    normalize: true,
    maxInputTokens: 256,
    maxBatchSize: 4,
  },

  "local/potion-base-8m": {
    backend: "model2vec",
    reference: "local/potion-base-8m",
    provider: "local",
    model: "potion-base-8m",
    repo: "minishlab/potion-base-8M",
    revision: "bf8b056651a2c21b8d2565580b8569da283cab23",
    modelFile: "model.safetensors",
    embeddingTensor: "embeddings",
    tokenizerFile: "tokenizer.json",
    dimension: 256,
    metric: "cosine",
    normalize: true,
    maxInputTokens: 512,
    maxBatchSize: 256,
  },

  "local/potion-code-16m-v2": {
    backend: "model2vec",
    reference: "local/potion-code-16m-v2",
    provider: "local",
    model: "potion-code-16m-v2",
    repo: "minishlab/potion-code-16M-v2",
    revision: "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b",
    modelFile: "model.safetensors",
    embeddingTensor: "embeddings",
    tokenizerFile: "tokenizer.json",
    dimension: 256,
    metric: "cosine",
    normalize: true,
    maxInputTokens: 8192,
    maxBatchSize: 256,
  },

  "local/multilingual-e5-small": {
    backend: "transformers-js",
    reference: "local/multilingual-e5-small",
    provider: "local",
    model: "multilingual-e5-small",
    repo: "Xenova/multilingual-e5-small",
    revision: "761b726dd34fb83930e26aab4e9ac3899aa1fa78",
    dtype: "q8",
    dimension: 384,
    metric: "cosine",
    pooling: "mean",
    normalize: true,
    queryPrefix: "query: ",
    documentPrefix: "passage: ",
    maxInputTokens: 512,
    maxBatchSize: 4,
  },

  "local/jina-embeddings-v2-base-code": {
    backend: "transformers-js",
    reference: "local/jina-embeddings-v2-base-code",
    provider: "local",
    model: "jina-embeddings-v2-base-code",
    repo: "jinaai/jina-embeddings-v2-base-code",
    revision: "516f4baf13dec4ddddda8631e019b5737c8bc250",
    dtype: "q8",
    dimension: 768,
    metric: "cosine",
    pooling: "mean",
    normalize: true,
    maxInputTokens: 8192,
    maxBatchSize: 2,
  },

  "local/gte-modernbert-base": {
    backend: "transformers-js",
    reference: "local/gte-modernbert-base",
    provider: "local",
    model: "gte-modernbert-base",
    repo: "Alibaba-NLP/gte-modernbert-base",
    revision: "e7f32e3c00f91d699e8c43b53106206bcc72bb22",
    dtype: "q4",
    dimension: 768,
    metric: "cosine",
    pooling: "cls",
    normalize: true,
    maxInputTokens: 8192,
    maxBatchSize: 2,
  },

  "local/nomic-embed-text-v1.5": {
    backend: "transformers-js",
    reference: "local/nomic-embed-text-v1.5",
    provider: "local",
    model: "nomic-embed-text-v1.5",
    repo: "nomic-ai/nomic-embed-text-v1.5",
    revision: "e9b6763023c676ca8431644204f50c2b100d9aab",
    dtype: "q4",
    dimension: 768,
    metric: "cosine",
    pooling: "mean",
    normalize: true,
    queryPrefix: "search_query: ",
    documentPrefix: "search_document: ",
    maxInputTokens: 8192,
    maxBatchSize: 2,
  },
} as const;

export type EmbeddingCatalogEntry =
  (typeof EMBEDDING_MODEL_CATALOG)[keyof typeof EMBEDDING_MODEL_CATALOG];

export type LlamaCppEmbeddingCatalogEntry = Extract<
  EmbeddingCatalogEntry,
  { backend: "llama-cpp" }
>;

export type TransformersJsEmbeddingCatalogEntry = Extract<
  EmbeddingCatalogEntry,
  { backend: "transformers-js" }
>;

export type Model2VecEmbeddingCatalogEntry = Extract<
  EmbeddingCatalogEntry,
  { backend: "model2vec" }
>;

export type QwenEmbeddingCatalogEntry = Extract<
  EmbeddingCatalogEntry,
  { backend: "qwen" }
>;

export type QwenTextEmbeddingCatalogEntry = Extract<
  QwenEmbeddingCatalogEntry,
  { kind: "text" }
>;

export type QwenMultimodalEmbeddingCatalogEntry = Extract<
  QwenEmbeddingCatalogEntry,
  { kind: "multimodal" }
>;

export type EmbeddingModelCatalogId = keyof typeof EMBEDDING_MODEL_CATALOG;

export function listEmbeddingModels(): EmbeddingCatalogEntry[] {
  return Object.values(EMBEDDING_MODEL_CATALOG);
}

export function getEmbeddingModelCatalogEntry(
  reference: string,
): EmbeddingCatalogEntry | undefined {
  return EMBEDDING_MODEL_CATALOG[reference as EmbeddingModelCatalogId];
}
