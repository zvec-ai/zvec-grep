const DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT =
  "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings";
const DEFAULT_QWEN3_VL_EMBEDDING_ENDPOINT =
  "https://dashscope.aliyuncs.com/api/v1/services/embeddings/multimodal-embedding/multimodal-embedding";

export const EMBEDDING_MODEL_CATALOG = {
  "local/embeddinggemma-300m": {
    backend: "llama-cpp",
    reference: "local/embeddinggemma-300m",
    provider: "local",
    model: "embeddinggemma-300m",
    uri: "hf:ggml-org/embeddinggemma-300M-GGUF/embeddinggemma-300M-Q8_0.gguf#0f741b5a6585bd53aeb15cd1372c56f2a0f65e12",
    cacheFile: "hf_ggml-org_embeddinggemma-300M-Q8_0.gguf",
    sources: {
      huggingFace: {
        repo: "ggml-org/embeddinggemma-300M-GGUF",
        revision: "0f741b5a6585bd53aeb15cd1372c56f2a0f65e12",
      },
      modelScope: {
        repo: "ggml-org/embeddinggemma-300M-GGUF",
        revision: "e2fab2963ac0943b1dc320cf0e267bd0e06e2e97",
      },
    },
    artifacts: [
      {
        path: "embeddinggemma-300M-Q8_0.gguf",
        size: 333590944,
        sha256:
          "b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63",
      },
    ],
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
    uri: "hf:Qwen/Qwen3-Embedding-0.6B-GGUF/Qwen3-Embedding-0.6B-Q8_0.gguf#370f27d7550e0def9b39c1f16d3fbaa13aa67728",
    cacheFile: "hf_Qwen_Qwen3-Embedding-0.6B-Q8_0.gguf",
    sources: {
      huggingFace: {
        repo: "Qwen/Qwen3-Embedding-0.6B-GGUF",
        revision: "370f27d7550e0def9b39c1f16d3fbaa13aa67728",
      },
      modelScope: {
        repo: "Qwen/Qwen3-Embedding-0.6B-GGUF",
        revision: "61ae123505fc6fa7f36d372fae1a30bc384241ea",
      },
    },
    artifacts: [
      {
        path: "Qwen3-Embedding-0.6B-Q8_0.gguf",
        size: 639150592,
        sha256:
          "06507c7b42688469c4e7298b0a1e16deff06caf291cf0a5b278c308249c3e439",
      },
    ],
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
    defaultEndpoint: DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT,
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
    defaultEndpoint: DEFAULT_QWEN_TEXT_EMBEDDING_ENDPOINT,
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
    defaultEndpoint: DEFAULT_QWEN3_VL_EMBEDDING_ENDPOINT,
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
    sources: {
      huggingFace: {
        repo: "onnx-community/bge-small-en-v1.5-ONNX",
        revision: "4a9a46c7b88fa408e650a571a1800243f26309bd",
      },
      modelScope: {
        repo: "onnx-community/bge-small-en-v1.5-ONNX",
        revision: "f246b360b061b613fe0b449f2e14de12f875dad7",
      },
    },
    artifacts: [
      {
        path: "config.json",
        size: 867,
        sha256:
          "26ad1d93a1ba37422fc25472191cfa230010631fa6a01f9d9f81fa13df2d0917",
      },
      {
        path: "tokenizer.json",
        size: 533603,
        sha256:
          "ea77de727ef7fd34d177b83b4b1f1d3bb8884c95c90b6554a0adb0b3b65350a9",
      },
      {
        path: "tokenizer_config.json",
        size: 1271,
        sha256:
          "eebe14d184cfbd65f6a11d2a5ff39385c4044c8a670a89acf1a13331e04faa60",
      },
      {
        path: "onnx/model_q4.onnx",
        size: 132562,
        sha256:
          "266acb3edd98a1932f876d15bd8f7881a4955d0d53a6d5e79900f788b432de09",
      },
      {
        path: "onnx/model_q4.onnx_data",
        size: 61185024,
        sha256:
          "77aff1dfb1e0591a40b61a91e5a97796f14c2aff56706e345bef5cbb3613b8cc",
      },
    ],
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
    sources: {
      huggingFace: {
        repo: "onnx-community/all-MiniLM-L6-v2-ONNX",
        revision: "aff7a1dc4e8a1ea593e6ea21e95c22ef0a25966f",
      },
      modelScope: {
        repo: "onnx-community/all-MiniLM-L6-v2-ONNX",
        revision: "e1da369847063d70f2fd772226551865bcab1c2d",
      },
    },
    artifacts: [
      {
        path: "config.json",
        size: 794,
        sha256:
          "fe5da868b77bdb104140822a5af0837cb6450ad6de8ff3dfcc8dd44ddd3e3ae7",
      },
      {
        path: "tokenizer.json",
        size: 533808,
        sha256:
          "07805d116826679de90b4edeb2222269c4b8753bc0981be4399f732b2708e904",
      },
      {
        path: "tokenizer_config.json",
        size: 1463,
        sha256:
          "e10bb633ba0d7f69ed342ae7de607f36b39ce53b455fbda69c71700bf57e6f66",
      },
      {
        path: "onnx/model_q4.onnx",
        size: 69663,
        sha256:
          "e4dcb918111189b7686147e309379832fce83d4ecbf17c395961749b5788c786",
      },
      {
        path: "onnx/model_q4.onnx_data",
        size: 54429696,
        sha256:
          "56fb7a55115e900196115a74e399beb45c2f41ae00b99525d46fb52935c4ee2a",
      },
    ],
    dtype: "q4",
    dimension: 384,
    metric: "cosine",
    pooling: "mean",
    normalize: true,
    maxInputTokens: 256,
    maxBatchSize: 4,
  },

  "local/potion-retrieval-32m": {
    backend: "model2vec",
    reference: "local/potion-retrieval-32m",
    provider: "local",
    model: "potion-retrieval-32m",
    repo: "minishlab/potion-retrieval-32M",
    revision: "6fc8051fab2a1e0ee76689cf08c853792ac285e7",
    sources: {
      huggingFace: {
        repo: "minishlab/potion-retrieval-32M",
        revision: "6fc8051fab2a1e0ee76689cf08c853792ac285e7",
      },
      modelScope: {
        repo: "minishlab/potion-retrieval-32M",
        revision: "33da23fc75cb732b5370bf25adde3db74b0d65b3",
      },
    },
    artifacts: [
      {
        path: "model.safetensors",
        size: 129210456,
        sha256:
          "07609e5bd33aad37900b3fd62f4ec96f6daec88ca4d46b9d8b928bfababf6ea0",
      },
      {
        path: "tokenizer.json",
        size: 1493150,
        sha256:
          "7d75cbc54318138807c401b0f0c9721117c628b39de8e8e0edb6cb17e0ee7d18",
      },
    ],
    modelFile: "model.safetensors",
    embeddingTensor: "embeddings",
    tokenizerFile: "tokenizer.json",
    dimension: 512,
    metric: "cosine",
    normalize: true,
    maxInputTokens: 1024,
    maxBatchSize: 256,
    defaultConcurrency: 2,
  },

  "local/potion-multilingual-128m": {
    backend: "model2vec",
    reference: "local/potion-multilingual-128m",
    provider: "local",
    model: "potion-multilingual-128m",
    repo: "minishlab/potion-multilingual-128M",
    revision: "73908c3438cf03b6a01bcb9611d62b23d0726f08",
    sources: {
      huggingFace: {
        repo: "minishlab/potion-multilingual-128M",
        revision: "73908c3438cf03b6a01bcb9611d62b23d0726f08",
      },
      modelScope: {
        repo: "minishlab/potion-multilingual-128M",
        revision: "e8524678123f281add99f9745ac33d6604434dd7",
      },
    },
    artifacts: [
      {
        path: "model.safetensors",
        size: 512361560,
        sha256:
          "14b5eb39cb4ce5666da8ad1f3dc6be4346e9b2d601c073302fa0a31bf7943397",
      },
      {
        path: "tokenizer.json",
        size: 18616131,
        sha256:
          "19f1909063da3cfe3bd83a782381f040dccea475f4816de11116444a73e1b6a1",
      },
    ],
    modelFile: "model.safetensors",
    embeddingTensor: "embeddings",
    tokenizerFile: "tokenizer.json",
    dimension: 256,
    metric: "cosine",
    normalize: true,
    maxInputTokens: 1024,
    maxBatchSize: 256,
    defaultConcurrency: 2,
  },

  "local/potion-code-16m-v2": {
    backend: "model2vec",
    reference: "local/potion-code-16m-v2",
    provider: "local",
    model: "potion-code-16m-v2",
    repo: "minishlab/potion-code-16M-v2",
    revision: "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b",
    sources: {
      huggingFace: {
        repo: "minishlab/potion-code-16M-v2",
        revision: "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b",
      },
      modelScope: {
        repo: "minishlab/potion-code-16M-v2",
        revision: "3e922fde18f43b8db69f3381b6d468738d3dd2d7",
      },
    },
    artifacts: [
      {
        path: "model.safetensors",
        size: 32490072,
        sha256:
          "75cf7a6c2171b230ad19b1e7d8e0b1aee86da5a02af8e7cacedd9921d227623c",
      },
      {
        path: "tokenizer.json",
        size: 1024340,
        sha256:
          "107bbdcbad4bff1d299b7a4c3a2fb17c52890688b7dd0e4c9deab79d3c4f3d45",
      },
    ],
    modelFile: "model.safetensors",
    embeddingTensor: "embeddings",
    tokenizerFile: "tokenizer.json",
    dimension: 256,
    metric: "cosine",
    normalize: true,
    maxInputTokens: 1024,
    maxBatchSize: 256,
    defaultConcurrency: 2,
  },

  "local/multilingual-e5-small": {
    backend: "transformers-js",
    reference: "local/multilingual-e5-small",
    provider: "local",
    model: "multilingual-e5-small",
    repo: "Xenova/multilingual-e5-small",
    revision: "761b726dd34fb83930e26aab4e9ac3899aa1fa78",
    sources: {
      huggingFace: {
        repo: "Xenova/multilingual-e5-small",
        revision: "761b726dd34fb83930e26aab4e9ac3899aa1fa78",
      },
      modelScope: {
        repo: "Xenova/multilingual-e5-small",
        revision: "252d0dcb679dda2c7b6fd5bbfed15df3c7feaebf",
      },
    },
    artifacts: [
      {
        path: "config.json",
        size: 658,
        sha256:
          "cb99455288675345e1a4f411438d5d0adbba5fbd3a67ea4fb03c015433b996c1",
      },
      {
        path: "tokenizer.json",
        size: 17082730,
        sha256:
          "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
      },
      {
        path: "tokenizer_config.json",
        size: 443,
        sha256:
          "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b",
      },
      {
        path: "onnx/model_quantized.onnx",
        size: 118308185,
        sha256:
          "f80102d3f2a1229f387d3c81909990d8945513e347b0eab049f7de3c6f98c193",
      },
    ],
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
    sources: {
      huggingFace: {
        repo: "jinaai/jina-embeddings-v2-base-code",
        revision: "516f4baf13dec4ddddda8631e019b5737c8bc250",
      },
      modelScope: {
        repo: "jinaai/jina-embeddings-v2-base-code",
        revision: "91aa0a6aa801c408149324e32e8cd43f8502da8f",
      },
    },
    artifacts: [
      {
        path: "config.json",
        size: 1216,
        sha256:
          "e426aa684c7f9a95c5f020aa855faf93a24f065f5fad0c9e17b124670cabdea6",
      },
      {
        path: "tokenizer.json",
        size: 2561316,
        sha256:
          "b01c78a902aa4facb2f47f95449f48e2f7bbfea5d2472ee2f6ce92323c6f86e5",
      },
      {
        path: "tokenizer_config.json",
        size: 493,
        sha256:
          "f477aeb15ff9f78d3c1ddf2361d2b0b8b20cf55220f839f29a37f3a18efddd89",
      },
      {
        path: "onnx/model_quantized.onnx",
        size: 161895621,
        sha256:
          "ed45870251c9f0cf656e78aab0d37a23489066df8a222bb1c8caf8a45f2cb16d",
      },
    ],
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
    sources: {
      huggingFace: {
        repo: "Alibaba-NLP/gte-modernbert-base",
        revision: "e7f32e3c00f91d699e8c43b53106206bcc72bb22",
      },
      modelScope: {
        repo: "iic/gte-modernbert-base",
        revision: "678f4ed93760af288132f4f9dc5b6daebdc48777",
      },
    },
    artifacts: [
      {
        path: "config.json",
        size: 1184,
        sha256:
          "8ba54dc3d35d7194f5178a4194b649f146753e02dabd22bdca5c5cbac15069ed",
      },
      {
        path: "tokenizer.json",
        size: 3583228,
        sha256:
          "6c8aaa9a542084f2457eab775d4eeb51f92a70c0fd9de28d5edb0ddec3c08d30",
      },
      {
        path: "tokenizer_config.json",
        size: 20867,
        sha256:
          "9654072f7c873161814043cf08cb5ed72f71d0b935abcd4e267935cb34352c21",
      },
      {
        path: "onnx/model_q4.onnx",
        size: 224152761,
        sha256:
          "5d1278a1ba749c06b82f9a2f65c2c1c5765d36f2eb88b4888de62ff12b0724a2",
      },
    ],
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
    sources: {
      huggingFace: {
        repo: "nomic-ai/nomic-embed-text-v1.5",
        revision: "e9b6763023c676ca8431644204f50c2b100d9aab",
      },
      modelScope: {
        repo: "nomic-ai/nomic-embed-text-v1.5",
        revision: "c6fb77fdf73531ee8319b34e46f7e749b59e74e8",
      },
    },
    artifacts: [
      {
        path: "config.json",
        size: 2538,
        sha256:
          "9ab00bd92cee80a569f708140b7b6c1661a65891ff3765b1519e181ba2f2c92b",
      },
      {
        path: "tokenizer.json",
        size: 711396,
        sha256:
          "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
      },
      {
        path: "tokenizer_config.json",
        size: 1191,
        sha256:
          "d7e0000bcc80134debd2222220427e6bf5fa20a669f40a0d0d1409cc18e0a9bc",
      },
      {
        path: "onnx/model_q4.onnx",
        size: 165113221,
        sha256:
          "314976b7b9fba83283f9c8a29ee680a159fa485f52104e3fa39d3d5858337003",
      },
    ],
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
