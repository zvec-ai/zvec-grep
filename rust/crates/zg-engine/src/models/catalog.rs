use crate::domain::model::Metric;

const DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT: &str =
    "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings";
const DEFAULT_QWEN3_VL_EMBEDDING_ENDPOINT: &str = "https://dashscope.aliyuncs.com/api/v1/services/embeddings/multimodal-embedding/multimodal-embedding";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactSourceConfig {
    pub(crate) repo: &'static str,
    pub(crate) revision: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactConfig {
    pub(crate) path: &'static str,
    pub(crate) size: u64,
    pub(crate) sha256: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactDownloadConfig {
    pub(crate) hugging_face: ArtifactSourceConfig,
    pub(crate) model_scope: ArtifactSourceConfig,
    pub(crate) artifacts: &'static [ArtifactConfig],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EmbeddingCatalogEntry {
    LlamaCpp(LlamaCppConfig),
    Qwen(QwenConfig),
    Transformers(TransformersConfig),
    Model2Vec(Model2VecConfig),
}

impl EmbeddingCatalogEntry {
    #[cfg(test)]
    pub(crate) const fn backend(self) -> &'static str {
        match self {
            Self::LlamaCpp(_) => "llama-cpp",
            Self::Qwen(_) => "qwen",
            Self::Transformers(_) => "transformers",
            Self::Model2Vec(_) => "model2vec",
        }
    }

    pub(crate) const fn reference(self) -> &'static str {
        match self {
            Self::LlamaCpp(entry) => entry.reference,
            Self::Qwen(entry) => entry.reference,
            Self::Transformers(entry) => entry.reference,
            Self::Model2Vec(entry) => entry.reference,
        }
    }

    #[cfg(test)]
    pub(crate) const fn dimension(self) -> usize {
        match self {
            Self::LlamaCpp(entry) => entry.dimension,
            Self::Qwen(entry) => entry.dimension,
            Self::Transformers(entry) => entry.dimension,
            Self::Model2Vec(entry) => entry.dimension,
        }
    }

    #[cfg(test)]
    pub(crate) const fn llama_cpp_config(self) -> Option<LlamaCppConfig> {
        if let Self::LlamaCpp(entry) = self {
            Some(entry)
        } else {
            None
        }
    }

    #[cfg(test)]
    pub(crate) const fn transformers_config(self) -> Option<TransformersConfig> {
        if let Self::Transformers(entry) = self {
            Some(entry)
        } else {
            None
        }
    }

    #[cfg(test)]
    pub(crate) const fn model2vec_config(self) -> Option<Model2VecConfig> {
        if let Self::Model2Vec(entry) = self {
            Some(entry)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LlamaCppConfig {
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) uri: &'static str,
    pub(crate) cache_file: &'static str,
    pub(crate) download: &'static ArtifactDownloadConfig,
    pub(crate) dimension: usize,
    pub(crate) metric: Metric,
    pub(crate) format: &'static str,
    pub(crate) context_size: usize,
    pub(crate) max_batch_size: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct QwenConfig {
    pub(crate) kind: &'static str,
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) dimension: usize,
    pub(crate) metric: Metric,
    pub(crate) default_endpoint: &'static str,
    pub(crate) max_batch_size: usize,
    pub(crate) max_input_tokens: usize,
    pub(crate) max_image_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransformersConfig {
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) repo: &'static str,
    pub(crate) revision: &'static str,
    pub(crate) download: &'static ArtifactDownloadConfig,
    pub(crate) dtype: &'static str,
    pub(crate) dimension: usize,
    pub(crate) metric: Metric,
    pub(crate) pooling: &'static str,
    pub(crate) normalize: bool,
    pub(crate) query_prefix: Option<&'static str>,
    pub(crate) document_prefix: Option<&'static str>,
    pub(crate) max_input_tokens: usize,
    pub(crate) max_batch_size: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Model2VecConfig {
    pub(crate) reference: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) repo: &'static str,
    pub(crate) revision: &'static str,
    pub(crate) download: &'static ArtifactDownloadConfig,
    pub(crate) model_file: &'static str,
    pub(crate) embedding_tensor: &'static str,
    pub(crate) tokenizer_file: &'static str,
    pub(crate) dimension: usize,
    pub(crate) metric: Metric,
    pub(crate) normalize: bool,
    pub(crate) max_input_tokens: usize,
    pub(crate) max_batch_size: usize,
    pub(crate) default_concurrency: usize,
    pub(crate) query_prefix: Option<&'static str>,
    pub(crate) document_prefix: Option<&'static str>,
}

const EMBEDDINGGEMMA_ARTIFACTS: &[ArtifactConfig] = &[ArtifactConfig {
    path: "embeddinggemma-300M-Q8_0.gguf",
    size: 333_590_944,
    sha256: "b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63",
}];

const QWEN3_GGUF_ARTIFACTS: &[ArtifactConfig] = &[ArtifactConfig {
    path: "Qwen3-Embedding-0.6B-Q8_0.gguf",
    size: 639_150_592,
    sha256: "06507c7b42688469c4e7298b0a1e16deff06caf291cf0a5b278c308249c3e439",
}];

const BGE_SMALL_ARTIFACTS: &[ArtifactConfig] = &[
    ArtifactConfig {
        path: "config.json",
        size: 867,
        sha256: "26ad1d93a1ba37422fc25472191cfa230010631fa6a01f9d9f81fa13df2d0917",
    },
    ArtifactConfig {
        path: "tokenizer.json",
        size: 533_603,
        sha256: "ea77de727ef7fd34d177b83b4b1f1d3bb8884c95c90b6554a0adb0b3b65350a9",
    },
    ArtifactConfig {
        path: "tokenizer_config.json",
        size: 1_271,
        sha256: "eebe14d184cfbd65f6a11d2a5ff39385c4044c8a670a89acf1a13331e04faa60",
    },
    ArtifactConfig {
        path: "onnx/model_q4.onnx",
        size: 132_562,
        sha256: "266acb3edd98a1932f876d15bd8f7881a4955d0d53a6d5e79900f788b432de09",
    },
    ArtifactConfig {
        path: "onnx/model_q4.onnx_data",
        size: 61_185_024,
        sha256: "77aff1dfb1e0591a40b61a91e5a97796f14c2aff56706e345bef5cbb3613b8cc",
    },
];

const MINILM_ARTIFACTS: &[ArtifactConfig] = &[
    ArtifactConfig {
        path: "config.json",
        size: 794,
        sha256: "fe5da868b77bdb104140822a5af0837cb6450ad6de8ff3dfcc8dd44ddd3e3ae7",
    },
    ArtifactConfig {
        path: "tokenizer.json",
        size: 533_808,
        sha256: "07805d116826679de90b4edeb2222269c4b8753bc0981be4399f732b2708e904",
    },
    ArtifactConfig {
        path: "tokenizer_config.json",
        size: 1_463,
        sha256: "e10bb633ba0d7f69ed342ae7de607f36b39ce53b455fbda69c71700bf57e6f66",
    },
    ArtifactConfig {
        path: "onnx/model_q4.onnx",
        size: 69_663,
        sha256: "e4dcb918111189b7686147e309379832fce83d4ecbf17c395961749b5788c786",
    },
    ArtifactConfig {
        path: "onnx/model_q4.onnx_data",
        size: 54_429_696,
        sha256: "56fb7a55115e900196115a74e399beb45c2f41ae00b99525d46fb52935c4ee2a",
    },
];

const POTION_RETRIEVAL_ARTIFACTS: &[ArtifactConfig] = &[
    ArtifactConfig {
        path: "model.safetensors",
        size: 129_210_456,
        sha256: "07609e5bd33aad37900b3fd62f4ec96f6daec88ca4d46b9d8b928bfababf6ea0",
    },
    ArtifactConfig {
        path: "tokenizer.json",
        size: 1_493_150,
        sha256: "7d75cbc54318138807c401b0f0c9721117c628b39de8e8e0edb6cb17e0ee7d18",
    },
];

const POTION_MULTILINGUAL_ARTIFACTS: &[ArtifactConfig] = &[
    ArtifactConfig {
        path: "model.safetensors",
        size: 512_361_560,
        sha256: "14b5eb39cb4ce5666da8ad1f3dc6be4346e9b2d601c073302fa0a31bf7943397",
    },
    ArtifactConfig {
        path: "tokenizer.json",
        size: 18_616_131,
        sha256: "19f1909063da3cfe3bd83a782381f040dccea475f4816de11116444a73e1b6a1",
    },
];

const POTION_CODE_ARTIFACTS: &[ArtifactConfig] = &[
    ArtifactConfig {
        path: "model.safetensors",
        size: 32_490_072,
        sha256: "75cf7a6c2171b230ad19b1e7d8e0b1aee86da5a02af8e7cacedd9921d227623c",
    },
    ArtifactConfig {
        path: "tokenizer.json",
        size: 1_024_340,
        sha256: "107bbdcbad4bff1d299b7a4c3a2fb17c52890688b7dd0e4c9deab79d3c4f3d45",
    },
];

const MULTILINGUAL_E5_ARTIFACTS: &[ArtifactConfig] = &[
    ArtifactConfig {
        path: "config.json",
        size: 658,
        sha256: "cb99455288675345e1a4f411438d5d0adbba5fbd3a67ea4fb03c015433b996c1",
    },
    ArtifactConfig {
        path: "tokenizer.json",
        size: 17_082_730,
        sha256: "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
    },
    ArtifactConfig {
        path: "tokenizer_config.json",
        size: 443,
        sha256: "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b",
    },
    ArtifactConfig {
        path: "onnx/model_quantized.onnx",
        size: 118_308_185,
        sha256: "f80102d3f2a1229f387d3c81909990d8945513e347b0eab049f7de3c6f98c193",
    },
];

const JINA_CODE_ARTIFACTS: &[ArtifactConfig] = &[
    ArtifactConfig {
        path: "config.json",
        size: 1_216,
        sha256: "e426aa684c7f9a95c5f020aa855faf93a24f065f5fad0c9e17b124670cabdea6",
    },
    ArtifactConfig {
        path: "tokenizer.json",
        size: 2_561_316,
        sha256: "b01c78a902aa4facb2f47f95449f48e2f7bbfea5d2472ee2f6ce92323c6f86e5",
    },
    ArtifactConfig {
        path: "tokenizer_config.json",
        size: 493,
        sha256: "f477aeb15ff9f78d3c1ddf2361d2b0b8b20cf55220f839f29a37f3a18efddd89",
    },
    ArtifactConfig {
        path: "onnx/model_quantized.onnx",
        size: 161_895_621,
        sha256: "ed45870251c9f0cf656e78aab0d37a23489066df8a222bb1c8caf8a45f2cb16d",
    },
];

const GTE_ARTIFACTS: &[ArtifactConfig] = &[
    ArtifactConfig {
        path: "config.json",
        size: 1_184,
        sha256: "8ba54dc3d35d7194f5178a4194b649f146753e02dabd22bdca5c5cbac15069ed",
    },
    ArtifactConfig {
        path: "tokenizer.json",
        size: 3_583_228,
        sha256: "6c8aaa9a542084f2457eab775d4eeb51f92a70c0fd9de28d5edb0ddec3c08d30",
    },
    ArtifactConfig {
        path: "tokenizer_config.json",
        size: 20_867,
        sha256: "9654072f7c873161814043cf08cb5ed72f71d0b935abcd4e267935cb34352c21",
    },
    ArtifactConfig {
        path: "onnx/model_q4.onnx",
        size: 224_152_761,
        sha256: "5d1278a1ba749c06b82f9a2f65c2c1c5765d36f2eb88b4888de62ff12b0724a2",
    },
];

const NOMIC_ARTIFACTS: &[ArtifactConfig] = &[
    ArtifactConfig {
        path: "config.json",
        size: 2_538,
        sha256: "9ab00bd92cee80a569f708140b7b6c1661a65891ff3765b1519e181ba2f2c92b",
    },
    ArtifactConfig {
        path: "tokenizer.json",
        size: 711_396,
        sha256: "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
    },
    ArtifactConfig {
        path: "tokenizer_config.json",
        size: 1_191,
        sha256: "d7e0000bcc80134debd2222220427e6bf5fa20a669f40a0d0d1409cc18e0a9bc",
    },
    ArtifactConfig {
        path: "onnx/model_q4.onnx",
        size: 165_113_221,
        sha256: "314976b7b9fba83283f9c8a29ee680a159fa485f52104e3fa39d3d5858337003",
    },
];

const CATALOG: [EmbeddingCatalogEntry; 14] = [
    EmbeddingCatalogEntry::LlamaCpp(LlamaCppConfig {
        reference: "local/embeddinggemma-300m",
        provider: "local",
        model: "embeddinggemma-300m",
        uri: "hf:ggml-org/embeddinggemma-300M-GGUF/embeddinggemma-300M-Q8_0.gguf#0f741b5a6585bd53aeb15cd1372c56f2a0f65e12",
        cache_file: "hf_ggml-org_embeddinggemma-300M-Q8_0.gguf",
        download: &ArtifactDownloadConfig {
            hugging_face: ArtifactSourceConfig {
                repo: "ggml-org/embeddinggemma-300M-GGUF",
                revision: "0f741b5a6585bd53aeb15cd1372c56f2a0f65e12",
            },
            model_scope: ArtifactSourceConfig {
                repo: "ggml-org/embeddinggemma-300M-GGUF",
                revision: "e2fab2963ac0943b1dc320cf0e267bd0e06e2e97",
            },
            artifacts: EMBEDDINGGEMMA_ARTIFACTS,
        },
        dimension: 768,
        metric: Metric::Cosine,
        format: "embeddinggemma",
        context_size: 2_048,
        max_batch_size: 16,
    }),
    EmbeddingCatalogEntry::LlamaCpp(LlamaCppConfig {
        reference: "local/qwen3-embedding-0.6b",
        provider: "local",
        model: "qwen3-embedding-0.6b",
        uri: "hf:Qwen/Qwen3-Embedding-0.6B-GGUF/Qwen3-Embedding-0.6B-Q8_0.gguf#370f27d7550e0def9b39c1f16d3fbaa13aa67728",
        cache_file: "hf_Qwen_Qwen3-Embedding-0.6B-Q8_0.gguf",
        download: &ArtifactDownloadConfig {
            hugging_face: ArtifactSourceConfig {
                repo: "Qwen/Qwen3-Embedding-0.6B-GGUF",
                revision: "370f27d7550e0def9b39c1f16d3fbaa13aa67728",
            },
            model_scope: ArtifactSourceConfig {
                repo: "Qwen/Qwen3-Embedding-0.6B-GGUF",
                revision: "61ae123505fc6fa7f36d372fae1a30bc384241ea",
            },
            artifacts: QWEN3_GGUF_ARTIFACTS,
        },
        dimension: 1_024,
        metric: Metric::Cosine,
        format: "qwen3",
        context_size: 8_192,
        max_batch_size: 8,
    }),
    EmbeddingCatalogEntry::Qwen(QwenConfig {
        kind: "text",
        reference: "qwen/text-embedding-v4",
        provider: "qwen",
        model: "text-embedding-v4",
        dimension: 1_024,
        metric: Metric::Cosine,
        default_endpoint: DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT,
        max_batch_size: 10,
        max_input_tokens: 8_192,
        max_image_bytes: None,
    }),
    EmbeddingCatalogEntry::Qwen(QwenConfig {
        kind: "text",
        reference: "qwen/qwen3.7-text-embedding",
        provider: "qwen",
        model: "qwen3.7-text-embedding",
        dimension: 1_024,
        metric: Metric::Cosine,
        default_endpoint: DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT,
        max_batch_size: 20,
        max_input_tokens: 128_000,
        max_image_bytes: None,
    }),
    EmbeddingCatalogEntry::Qwen(QwenConfig {
        kind: "multimodal",
        reference: "qwen/qwen3-vl-embedding",
        provider: "qwen",
        model: "qwen3-vl-embedding",
        dimension: 2_560,
        metric: Metric::Cosine,
        default_endpoint: DEFAULT_QWEN3_VL_EMBEDDING_ENDPOINT,
        max_batch_size: 20,
        max_input_tokens: 32_000,
        max_image_bytes: Some(10 * 1_024 * 1_024),
    }),
    EmbeddingCatalogEntry::Transformers(TransformersConfig {
        reference: "local/bge-small-en-v1.5",
        provider: "local",
        model: "bge-small-en-v1.5",
        repo: "onnx-community/bge-small-en-v1.5-ONNX",
        revision: "4a9a46c7b88fa408e650a571a1800243f26309bd",
        download: &ArtifactDownloadConfig {
            hugging_face: ArtifactSourceConfig {
                repo: "onnx-community/bge-small-en-v1.5-ONNX",
                revision: "4a9a46c7b88fa408e650a571a1800243f26309bd",
            },
            model_scope: ArtifactSourceConfig {
                repo: "onnx-community/bge-small-en-v1.5-ONNX",
                revision: "f246b360b061b613fe0b449f2e14de12f875dad7",
            },
            artifacts: BGE_SMALL_ARTIFACTS,
        },
        dtype: "q4",
        dimension: 384,
        metric: Metric::Cosine,
        pooling: "cls",
        normalize: true,
        query_prefix: Some("Represent this sentence for searching relevant passages: "),
        document_prefix: None,
        max_input_tokens: 512,
        max_batch_size: 4,
    }),
    EmbeddingCatalogEntry::Transformers(TransformersConfig {
        reference: "local/all-minilm-l6-v2",
        provider: "local",
        model: "all-minilm-l6-v2",
        repo: "onnx-community/all-MiniLM-L6-v2-ONNX",
        revision: "aff7a1dc4e8a1ea593e6ea21e95c22ef0a25966f",
        download: &ArtifactDownloadConfig {
            hugging_face: ArtifactSourceConfig {
                repo: "onnx-community/all-MiniLM-L6-v2-ONNX",
                revision: "aff7a1dc4e8a1ea593e6ea21e95c22ef0a25966f",
            },
            model_scope: ArtifactSourceConfig {
                repo: "onnx-community/all-MiniLM-L6-v2-ONNX",
                revision: "e1da369847063d70f2fd772226551865bcab1c2d",
            },
            artifacts: MINILM_ARTIFACTS,
        },
        dtype: "q4",
        dimension: 384,
        metric: Metric::Cosine,
        pooling: "mean",
        normalize: true,
        query_prefix: None,
        document_prefix: None,
        max_input_tokens: 256,
        max_batch_size: 4,
    }),
    EmbeddingCatalogEntry::Model2Vec(Model2VecConfig {
        reference: "local/potion-retrieval-32m",
        provider: "local",
        model: "potion-retrieval-32m",
        repo: "minishlab/potion-retrieval-32M",
        revision: "6fc8051fab2a1e0ee76689cf08c853792ac285e7",
        download: &ArtifactDownloadConfig {
            hugging_face: ArtifactSourceConfig {
                repo: "minishlab/potion-retrieval-32M",
                revision: "6fc8051fab2a1e0ee76689cf08c853792ac285e7",
            },
            model_scope: ArtifactSourceConfig {
                repo: "minishlab/potion-retrieval-32M",
                revision: "33da23fc75cb732b5370bf25adde3db74b0d65b3",
            },
            artifacts: POTION_RETRIEVAL_ARTIFACTS,
        },
        model_file: "model.safetensors",
        embedding_tensor: "embeddings",
        tokenizer_file: "tokenizer.json",
        dimension: 512,
        metric: Metric::Cosine,
        normalize: true,
        max_input_tokens: 1_024,
        max_batch_size: 256,
        default_concurrency: 2,
        query_prefix: None,
        document_prefix: None,
    }),
    EmbeddingCatalogEntry::Model2Vec(Model2VecConfig {
        reference: "local/potion-multilingual-128m",
        provider: "local",
        model: "potion-multilingual-128m",
        repo: "minishlab/potion-multilingual-128M",
        revision: "73908c3438cf03b6a01bcb9611d62b23d0726f08",
        download: &ArtifactDownloadConfig {
            hugging_face: ArtifactSourceConfig {
                repo: "minishlab/potion-multilingual-128M",
                revision: "73908c3438cf03b6a01bcb9611d62b23d0726f08",
            },
            model_scope: ArtifactSourceConfig {
                repo: "minishlab/potion-multilingual-128M",
                revision: "e8524678123f281add99f9745ac33d6604434dd7",
            },
            artifacts: POTION_MULTILINGUAL_ARTIFACTS,
        },
        model_file: "model.safetensors",
        embedding_tensor: "embeddings",
        tokenizer_file: "tokenizer.json",
        dimension: 256,
        metric: Metric::Cosine,
        normalize: true,
        max_input_tokens: 1_024,
        max_batch_size: 256,
        default_concurrency: 2,
        query_prefix: None,
        document_prefix: None,
    }),
    EmbeddingCatalogEntry::Model2Vec(Model2VecConfig {
        reference: "local/potion-code-16m-v2",
        provider: "local",
        model: "potion-code-16m-v2",
        repo: "minishlab/potion-code-16M-v2",
        revision: "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b",
        download: &ArtifactDownloadConfig {
            hugging_face: ArtifactSourceConfig {
                repo: "minishlab/potion-code-16M-v2",
                revision: "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b",
            },
            model_scope: ArtifactSourceConfig {
                repo: "minishlab/potion-code-16M-v2",
                revision: "3e922fde18f43b8db69f3381b6d468738d3dd2d7",
            },
            artifacts: POTION_CODE_ARTIFACTS,
        },
        model_file: "model.safetensors",
        embedding_tensor: "embeddings",
        tokenizer_file: "tokenizer.json",
        dimension: 256,
        metric: Metric::Cosine,
        normalize: true,
        max_input_tokens: 1_024,
        max_batch_size: 256,
        default_concurrency: 2,
        query_prefix: None,
        document_prefix: None,
    }),
    EmbeddingCatalogEntry::Transformers(TransformersConfig {
        reference: "local/multilingual-e5-small",
        provider: "local",
        model: "multilingual-e5-small",
        repo: "Xenova/multilingual-e5-small",
        revision: "761b726dd34fb83930e26aab4e9ac3899aa1fa78",
        download: &ArtifactDownloadConfig {
            hugging_face: ArtifactSourceConfig {
                repo: "Xenova/multilingual-e5-small",
                revision: "761b726dd34fb83930e26aab4e9ac3899aa1fa78",
            },
            model_scope: ArtifactSourceConfig {
                repo: "Xenova/multilingual-e5-small",
                revision: "252d0dcb679dda2c7b6fd5bbfed15df3c7feaebf",
            },
            artifacts: MULTILINGUAL_E5_ARTIFACTS,
        },
        dtype: "q8",
        dimension: 384,
        metric: Metric::Cosine,
        pooling: "mean",
        normalize: true,
        query_prefix: Some("query: "),
        document_prefix: Some("passage: "),
        max_input_tokens: 512,
        max_batch_size: 4,
    }),
    EmbeddingCatalogEntry::Transformers(TransformersConfig {
        reference: "local/jina-embeddings-v2-base-code",
        provider: "local",
        model: "jina-embeddings-v2-base-code",
        repo: "jinaai/jina-embeddings-v2-base-code",
        revision: "516f4baf13dec4ddddda8631e019b5737c8bc250",
        download: &ArtifactDownloadConfig {
            hugging_face: ArtifactSourceConfig {
                repo: "jinaai/jina-embeddings-v2-base-code",
                revision: "516f4baf13dec4ddddda8631e019b5737c8bc250",
            },
            model_scope: ArtifactSourceConfig {
                repo: "jinaai/jina-embeddings-v2-base-code",
                revision: "91aa0a6aa801c408149324e32e8cd43f8502da8f",
            },
            artifacts: JINA_CODE_ARTIFACTS,
        },
        dtype: "q8",
        dimension: 768,
        metric: Metric::Cosine,
        pooling: "mean",
        normalize: true,
        query_prefix: None,
        document_prefix: None,
        max_input_tokens: 8_192,
        max_batch_size: 2,
    }),
    EmbeddingCatalogEntry::Transformers(TransformersConfig {
        reference: "local/gte-modernbert-base",
        provider: "local",
        model: "gte-modernbert-base",
        repo: "Alibaba-NLP/gte-modernbert-base",
        revision: "e7f32e3c00f91d699e8c43b53106206bcc72bb22",
        download: &ArtifactDownloadConfig {
            hugging_face: ArtifactSourceConfig {
                repo: "Alibaba-NLP/gte-modernbert-base",
                revision: "e7f32e3c00f91d699e8c43b53106206bcc72bb22",
            },
            model_scope: ArtifactSourceConfig {
                repo: "iic/gte-modernbert-base",
                revision: "678f4ed93760af288132f4f9dc5b6daebdc48777",
            },
            artifacts: GTE_ARTIFACTS,
        },
        dtype: "q4",
        dimension: 768,
        metric: Metric::Cosine,
        pooling: "cls",
        normalize: true,
        query_prefix: None,
        document_prefix: None,
        max_input_tokens: 8_192,
        max_batch_size: 2,
    }),
    EmbeddingCatalogEntry::Transformers(TransformersConfig {
        reference: "local/nomic-embed-text-v1.5",
        provider: "local",
        model: "nomic-embed-text-v1.5",
        repo: "nomic-ai/nomic-embed-text-v1.5",
        revision: "e9b6763023c676ca8431644204f50c2b100d9aab",
        download: &ArtifactDownloadConfig {
            hugging_face: ArtifactSourceConfig {
                repo: "nomic-ai/nomic-embed-text-v1.5",
                revision: "e9b6763023c676ca8431644204f50c2b100d9aab",
            },
            model_scope: ArtifactSourceConfig {
                repo: "nomic-ai/nomic-embed-text-v1.5",
                revision: "c6fb77fdf73531ee8319b34e46f7e749b59e74e8",
            },
            artifacts: NOMIC_ARTIFACTS,
        },
        dtype: "q4",
        dimension: 768,
        metric: Metric::Cosine,
        pooling: "mean",
        normalize: true,
        query_prefix: Some("search_query: "),
        document_prefix: Some("search_document: "),
        max_input_tokens: 8_192,
        max_batch_size: 2,
    }),
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

        let minilm = get_embedding_model_catalog_entry("local/all-minilm-l6-v2")
            .expect("Transformers entry must exist");
        assert_eq!(minilm.backend(), "transformers");
        assert!(minilm.transformers_config().is_some());
    }
}
