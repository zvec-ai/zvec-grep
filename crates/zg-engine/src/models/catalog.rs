use super::spi::EmbeddingMetric;

const DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT: &str =
    "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings";
const DEFAULT_QWEN3_VL_EMBEDDING_ENDPOINT: &str = "https://dashscope.aliyuncs.com/api/v1/services/embeddings/multimodal-embedding/multimodal-embedding";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmbeddingCatalogEntry {
    LlamaCpp {
        reference: &'static str,
        provider: &'static str,
        model: &'static str,
        uri: &'static str,
        dimension: usize,
        metric: EmbeddingMetric,
        format: &'static str,
        context_size: usize,
        max_batch_size: usize,
    },
    Qwen {
        kind: &'static str,
        reference: &'static str,
        provider: &'static str,
        model: &'static str,
        dimension: usize,
        metric: EmbeddingMetric,
        default_endpoint: &'static str,
        max_batch_size: usize,
        max_input_tokens: usize,
        max_image_bytes: Option<usize>,
    },
    TransformersJs {
        reference: &'static str,
        provider: &'static str,
        model: &'static str,
        repo: &'static str,
        revision: &'static str,
        dtype: &'static str,
        dimension: usize,
        metric: EmbeddingMetric,
        pooling: &'static str,
        normalize: bool,
        query_prefix: Option<&'static str>,
        document_prefix: Option<&'static str>,
        max_input_tokens: usize,
        max_batch_size: usize,
    },
    Model2Vec {
        reference: &'static str,
        provider: &'static str,
        model: &'static str,
        repo: &'static str,
        revision: &'static str,
        model_file: &'static str,
        embedding_tensor: &'static str,
        tokenizer_file: &'static str,
        dimension: usize,
        metric: EmbeddingMetric,
        normalize: bool,
        max_input_tokens: usize,
        max_batch_size: usize,
        default_concurrency: usize,
    },
}

impl EmbeddingCatalogEntry {
    #[must_use]
    pub const fn backend(self) -> &'static str {
        match self {
            Self::LlamaCpp { .. } => "llama-cpp",
            Self::Qwen { .. } => "qwen",
            Self::TransformersJs { .. } => "transformers-js",
            Self::Model2Vec { .. } => "model2vec",
        }
    }

    #[must_use]
    pub const fn reference(self) -> &'static str {
        match self {
            Self::LlamaCpp { reference, .. }
            | Self::Qwen { reference, .. }
            | Self::TransformersJs { reference, .. }
            | Self::Model2Vec { reference, .. } => reference,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub const fn dimension(self) -> usize {
        match self {
            Self::LlamaCpp { dimension, .. }
            | Self::Qwen { dimension, .. }
            | Self::TransformersJs { dimension, .. }
            | Self::Model2Vec { dimension, .. } => dimension,
        }
    }

    pub(crate) const fn model2vec_config(self) -> Option<Model2VecConfig> {
        match self {
            Self::Model2Vec {
                reference,
                provider,
                model,
                repo,
                revision,
                model_file,
                embedding_tensor,
                tokenizer_file,
                dimension,
                metric,
                normalize,
                max_input_tokens,
                max_batch_size,
                default_concurrency,
            } => Some(Model2VecConfig {
                reference,
                provider,
                model,
                repo,
                revision,
                model_file,
                embedding_tensor,
                tokenizer_file,
                dimension,
                metric,
                normalize,
                max_input_tokens,
                max_batch_size,
                default_concurrency,
                query_prefix: None,
                document_prefix: None,
            }),
            Self::LlamaCpp { .. } | Self::Qwen { .. } | Self::TransformersJs { .. } => None,
        }
    }

    pub(crate) const fn llama_cpp_config(self) -> Option<LlamaCppConfig> {
        match self {
            Self::LlamaCpp {
                reference,
                provider,
                model,
                uri,
                dimension,
                metric,
                format,
                context_size,
                max_batch_size,
            } => Some(LlamaCppConfig {
                reference,
                provider,
                model,
                uri,
                dimension,
                metric,
                format,
                context_size,
                max_batch_size,
            }),
            Self::Qwen { .. } | Self::TransformersJs { .. } | Self::Model2Vec { .. } => None,
        }
    }

    pub(crate) const fn qwen_config(self) -> Option<QwenConfig> {
        match self {
            Self::Qwen {
                kind,
                reference,
                provider,
                model,
                dimension,
                metric,
                default_endpoint,
                max_batch_size,
                max_input_tokens,
                max_image_bytes,
            } => Some(QwenConfig {
                kind,
                reference,
                provider,
                model,
                dimension,
                metric,
                default_endpoint,
                max_batch_size,
                max_input_tokens,
                max_image_bytes,
            }),
            Self::LlamaCpp { .. } | Self::TransformersJs { .. } | Self::Model2Vec { .. } => None,
        }
    }

    pub(crate) const fn transformers_config(self) -> Option<TransformersConfig> {
        match self {
            Self::TransformersJs {
                reference,
                provider,
                model,
                repo,
                revision,
                dtype,
                dimension,
                metric,
                pooling,
                normalize,
                query_prefix,
                document_prefix,
                max_input_tokens,
                max_batch_size,
            } => Some(TransformersConfig {
                reference,
                provider,
                model,
                repo,
                revision,
                dtype,
                dimension,
                metric,
                pooling,
                normalize,
                query_prefix,
                document_prefix,
                max_input_tokens,
                max_batch_size,
            }),
            Self::LlamaCpp { .. } | Self::Qwen { .. } | Self::Model2Vec { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LlamaCppConfig {
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) uri: &'static str,
    pub(crate) dimension: usize,
    pub(crate) metric: EmbeddingMetric,
    pub(crate) format: &'static str,
    pub(crate) context_size: usize,
    pub(crate) max_batch_size: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct QwenConfig {
    pub(crate) kind: &'static str,
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) dimension: usize,
    pub(crate) metric: EmbeddingMetric,
    pub(crate) default_endpoint: &'static str,
    pub(crate) max_batch_size: usize,
    pub(crate) max_input_tokens: usize,
    pub(crate) max_image_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TransformersConfig {
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) repo: &'static str,
    pub(crate) revision: &'static str,
    pub(crate) dtype: &'static str,
    pub(crate) dimension: usize,
    pub(crate) metric: EmbeddingMetric,
    pub(crate) pooling: &'static str,
    pub(crate) normalize: bool,
    pub(crate) query_prefix: Option<&'static str>,
    pub(crate) document_prefix: Option<&'static str>,
    pub(crate) max_input_tokens: usize,
    pub(crate) max_batch_size: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Model2VecConfig {
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) repo: &'static str,
    pub(crate) revision: &'static str,
    pub(crate) model_file: &'static str,
    pub(crate) embedding_tensor: &'static str,
    pub(crate) tokenizer_file: &'static str,
    pub(crate) dimension: usize,
    pub(crate) metric: EmbeddingMetric,
    pub(crate) normalize: bool,
    pub(crate) max_input_tokens: usize,
    pub(crate) max_batch_size: usize,
    pub(crate) default_concurrency: usize,
    pub(crate) query_prefix: Option<&'static str>,
    pub(crate) document_prefix: Option<&'static str>,
}

const CATALOG: [EmbeddingCatalogEntry; 14] = [
    EmbeddingCatalogEntry::LlamaCpp {
        reference: "local/embeddinggemma-300m",
        provider: "local",
        model: "embeddinggemma-300m",
        uri: "hf:ggml-org/embeddinggemma-300M-GGUF/embeddinggemma-300M-Q8_0.gguf",
        dimension: 768,
        metric: EmbeddingMetric::Cosine,
        format: "embeddinggemma",
        context_size: 2_048,
        max_batch_size: 16,
    },
    EmbeddingCatalogEntry::LlamaCpp {
        reference: "local/qwen3-embedding-0.6b",
        provider: "local",
        model: "qwen3-embedding-0.6b",
        uri: "hf:Qwen/Qwen3-Embedding-0.6B-GGUF/Qwen3-Embedding-0.6B-Q8_0.gguf",
        dimension: 1_024,
        metric: EmbeddingMetric::Cosine,
        format: "qwen3",
        context_size: 8_192,
        max_batch_size: 8,
    },
    EmbeddingCatalogEntry::Qwen {
        kind: "text",
        reference: "qwen/text-embedding-v4",
        provider: "qwen",
        model: "text-embedding-v4",
        dimension: 1_024,
        metric: EmbeddingMetric::Cosine,
        default_endpoint: DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT,
        max_batch_size: 10,
        max_input_tokens: 8_192,
        max_image_bytes: None,
    },
    EmbeddingCatalogEntry::Qwen {
        kind: "text",
        reference: "qwen/qwen3.7-text-embedding",
        provider: "qwen",
        model: "qwen3.7-text-embedding",
        dimension: 1_024,
        metric: EmbeddingMetric::Cosine,
        default_endpoint: DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT,
        max_batch_size: 20,
        max_input_tokens: 128_000,
        max_image_bytes: None,
    },
    EmbeddingCatalogEntry::Qwen {
        kind: "multimodal",
        reference: "qwen/qwen3-vl-embedding",
        provider: "qwen",
        model: "qwen3-vl-embedding",
        dimension: 2_560,
        metric: EmbeddingMetric::Cosine,
        default_endpoint: DEFAULT_QWEN3_VL_EMBEDDING_ENDPOINT,
        max_batch_size: 20,
        max_input_tokens: 32_000,
        max_image_bytes: Some(10 * 1_024 * 1_024),
    },
    EmbeddingCatalogEntry::TransformersJs {
        reference: "local/bge-small-en-v1.5",
        provider: "local",
        model: "bge-small-en-v1.5",
        repo: "onnx-community/bge-small-en-v1.5-ONNX",
        revision: "4a9a46c7b88fa408e650a571a1800243f26309bd",
        dtype: "q4",
        dimension: 384,
        metric: EmbeddingMetric::Cosine,
        pooling: "cls",
        normalize: true,
        query_prefix: Some("Represent this sentence for searching relevant passages: "),
        document_prefix: None,
        max_input_tokens: 512,
        max_batch_size: 4,
    },
    EmbeddingCatalogEntry::TransformersJs {
        reference: "local/all-minilm-l6-v2",
        provider: "local",
        model: "all-minilm-l6-v2",
        repo: "onnx-community/all-MiniLM-L6-v2-ONNX",
        revision: "aff7a1dc4e8a1ea593e6ea21e95c22ef0a25966f",
        dtype: "q4",
        dimension: 384,
        metric: EmbeddingMetric::Cosine,
        pooling: "mean",
        normalize: true,
        query_prefix: None,
        document_prefix: None,
        max_input_tokens: 256,
        max_batch_size: 4,
    },
    EmbeddingCatalogEntry::Model2Vec {
        reference: "local/potion-retrieval-32m",
        provider: "local",
        model: "potion-retrieval-32m",
        repo: "minishlab/potion-retrieval-32M",
        revision: "6fc8051fab2a1e0ee76689cf08c853792ac285e7",
        model_file: "model.safetensors",
        embedding_tensor: "embeddings",
        tokenizer_file: "tokenizer.json",
        dimension: 512,
        metric: EmbeddingMetric::Cosine,
        normalize: true,
        max_input_tokens: 1_024,
        max_batch_size: 256,
        default_concurrency: 2,
    },
    EmbeddingCatalogEntry::Model2Vec {
        reference: "local/potion-multilingual-128m",
        provider: "local",
        model: "potion-multilingual-128m",
        repo: "minishlab/potion-multilingual-128M",
        revision: "73908c3438cf03b6a01bcb9611d62b23d0726f08",
        model_file: "model.safetensors",
        embedding_tensor: "embeddings",
        tokenizer_file: "tokenizer.json",
        dimension: 256,
        metric: EmbeddingMetric::Cosine,
        normalize: true,
        max_input_tokens: 1_024,
        max_batch_size: 256,
        default_concurrency: 2,
    },
    EmbeddingCatalogEntry::Model2Vec {
        reference: "local/potion-code-16m-v2",
        provider: "local",
        model: "potion-code-16m-v2",
        repo: "minishlab/potion-code-16M-v2",
        revision: "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b",
        model_file: "model.safetensors",
        embedding_tensor: "embeddings",
        tokenizer_file: "tokenizer.json",
        dimension: 256,
        metric: EmbeddingMetric::Cosine,
        normalize: true,
        max_input_tokens: 1_024,
        max_batch_size: 256,
        default_concurrency: 2,
    },
    EmbeddingCatalogEntry::TransformersJs {
        reference: "local/multilingual-e5-small",
        provider: "local",
        model: "multilingual-e5-small",
        repo: "Xenova/multilingual-e5-small",
        revision: "761b726dd34fb83930e26aab4e9ac3899aa1fa78",
        dtype: "q8",
        dimension: 384,
        metric: EmbeddingMetric::Cosine,
        pooling: "mean",
        normalize: true,
        query_prefix: Some("query: "),
        document_prefix: Some("passage: "),
        max_input_tokens: 512,
        max_batch_size: 4,
    },
    EmbeddingCatalogEntry::TransformersJs {
        reference: "local/jina-embeddings-v2-base-code",
        provider: "local",
        model: "jina-embeddings-v2-base-code",
        repo: "jinaai/jina-embeddings-v2-base-code",
        revision: "516f4baf13dec4ddddda8631e019b5737c8bc250",
        dtype: "q8",
        dimension: 768,
        metric: EmbeddingMetric::Cosine,
        pooling: "mean",
        normalize: true,
        query_prefix: None,
        document_prefix: None,
        max_input_tokens: 8_192,
        max_batch_size: 2,
    },
    EmbeddingCatalogEntry::TransformersJs {
        reference: "local/gte-modernbert-base",
        provider: "local",
        model: "gte-modernbert-base",
        repo: "Alibaba-NLP/gte-modernbert-base",
        revision: "e7f32e3c00f91d699e8c43b53106206bcc72bb22",
        dtype: "q4",
        dimension: 768,
        metric: EmbeddingMetric::Cosine,
        pooling: "cls",
        normalize: true,
        query_prefix: None,
        document_prefix: None,
        max_input_tokens: 8_192,
        max_batch_size: 2,
    },
    EmbeddingCatalogEntry::TransformersJs {
        reference: "local/nomic-embed-text-v1.5",
        provider: "local",
        model: "nomic-embed-text-v1.5",
        repo: "nomic-ai/nomic-embed-text-v1.5",
        revision: "e9b6763023c676ca8431644204f50c2b100d9aab",
        dtype: "q4",
        dimension: 768,
        metric: EmbeddingMetric::Cosine,
        pooling: "mean",
        normalize: true,
        query_prefix: Some("search_query: "),
        document_prefix: Some("search_document: "),
        max_input_tokens: 8_192,
        max_batch_size: 2,
    },
];

#[cfg(test)]
#[must_use]
pub fn list_embedding_models() -> Vec<EmbeddingCatalogEntry> {
    CATALOG.to_vec()
}

#[must_use]
pub fn get_embedding_model_catalog_entry(reference: &str) -> Option<EmbeddingCatalogEntry> {
    CATALOG
        .iter()
        .copied()
        .find(|entry| entry.reference() == reference)
}

#[cfg(test)]
mod tests {
    use super::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry, list_embedding_models};

    #[test]
    fn catalog_matches_typescript_order_and_model2vec_pins() {
        let references = list_embedding_models()
            .into_iter()
            .map(EmbeddingCatalogEntry::reference)
            .collect::<Vec<_>>();
        assert_eq!(
            references,
            [
                "local/embeddinggemma-300m",
                "local/qwen3-embedding-0.6b",
                "qwen/text-embedding-v4",
                "qwen/qwen3.7-text-embedding",
                "qwen/qwen3-vl-embedding",
                "local/bge-small-en-v1.5",
                "local/all-minilm-l6-v2",
                "local/potion-retrieval-32m",
                "local/potion-multilingual-128m",
                "local/potion-code-16m-v2",
                "local/multilingual-e5-small",
                "local/jina-embeddings-v2-base-code",
                "local/gte-modernbert-base",
                "local/nomic-embed-text-v1.5",
            ]
        );
        let code = get_embedding_model_catalog_entry("local/potion-code-16m-v2")
            .expect("TypeScript default Model2Vec entry must exist");
        assert_eq!(code.backend(), "model2vec");
        assert_eq!(code.dimension(), 256);
        let config = code
            .model2vec_config()
            .expect("default entry must be Model2Vec");
        assert_eq!(config.revision, "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b");
        assert_eq!(config.max_input_tokens, 1_024);
        assert_eq!(config.default_concurrency, 2);
    }
}
