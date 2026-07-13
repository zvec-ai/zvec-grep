use crate::{EngineError, Result};

#[cfg(feature = "llama-cpp")]
use std::{path::PathBuf, sync::Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingSchema {
    pub model: String,
    pub dimension: usize,
    /// Conservative character limit used by extraction before tokenization.
    pub max_input_chars: usize,
}

pub trait Embedder: Send + Sync {
    fn schema(&self) -> EmbeddingSchema;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Small, dependency-free embedding used for tests and offline fallback.
/// It hashes tokens into a normalized vector, so texts sharing words are close.
#[derive(Debug, Clone)]
pub struct DeterministicEmbedder {
    dimension: usize,
}

impl DeterministicEmbedder {
    pub fn new(dimension: usize) -> Result<Self> {
        if dimension < 8 {
            return Err(EngineError::InvalidConfig(
                "embedding dimension must be at least 8".into(),
            ));
        }
        Ok(Self { dimension })
    }

    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0; self.dimension];
        for token in text.split(|character: char| !character.is_alphanumeric()) {
            if token.is_empty() {
                continue;
            }
            let hash = blake3::hash(token.to_ascii_lowercase().as_bytes());
            let bytes = hash.as_bytes();
            let bucket =
                u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as usize % self.dimension;
            let sign = if bytes[8] & 1 == 0 { 1.0 } else { -1.0 };
            vector[bucket] += sign;
        }
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut vector {
                *value /= norm;
            }
        }
        vector
    }
}

impl Embedder for DeterministicEmbedder {
    fn schema(&self) -> EmbeddingSchema {
        EmbeddingSchema {
            model: "deterministic-token-hash".into(),
            dimension: self.dimension,
            max_input_chars: 3_600,
        }
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| self.embed_one(text)).collect())
    }
}

#[cfg(feature = "llama-cpp")]
#[derive(Debug, Clone)]
pub struct LlamaEmbeddingConfig {
    pub model_path: PathBuf,
    pub threads: usize,
    pub gpu_layers: u32,
    pub max_input_chars: usize,
}

#[cfg(feature = "llama-cpp")]
impl LlamaEmbeddingConfig {
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            threads: std::thread::available_parallelism().map_or(1, usize::from),
            gpu_layers: 0,
            // MiniLM is commonly configured for 256 tokens. Code tokenizes
            // densely, so this deliberately errs below four chars per token.
            max_input_chars: 900,
        }
    }
}

/// Batched GGUF embeddings powered by llama.cpp. A MiniLM GGUF can be used
/// here; the original Hugging Face safetensors/ONNX files cannot be loaded by
/// llama.cpp without conversion.
#[cfg(feature = "llama-cpp")]
pub struct LlamaEmbedder {
    model: Mutex<llama_cpp::LlamaModel>,
    schema: EmbeddingSchema,
    threads: u32,
}

#[cfg(feature = "llama-cpp")]
impl LlamaEmbedder {
    pub fn load(config: LlamaEmbeddingConfig) -> Result<Self> {
        let params = llama_cpp::LlamaParams {
            n_gpu_layers: config.gpu_layers,
            ..llama_cpp::LlamaParams::default()
        };
        let model = llama_cpp::LlamaModel::load_from_file(&config.model_path, params)
            .map_err(|error| EngineError::Embedding(error.to_string()))?;
        let dimension = model.embed_len();
        if dimension == 0 {
            return Err(EngineError::Embedding(
                "the GGUF model does not expose embeddings".into(),
            ));
        }
        Ok(Self {
            model: Mutex::new(model),
            schema: EmbeddingSchema {
                model: config.model_path.to_string_lossy().into_owned(),
                dimension,
                max_input_chars: config.max_input_chars,
            },
            threads: config.threads.max(1).min(u32::MAX as usize) as u32,
        })
    }
}

#[cfg(feature = "llama-cpp")]
impl Embedder for LlamaEmbedder {
    fn schema(&self) -> EmbeddingSchema {
        self.schema.clone()
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let params = llama_cpp::EmbeddingsParams {
            n_threads: self.threads,
            n_threads_batch: self.threads,
        };
        self.model
            .lock()
            .map_err(|_| EngineError::Embedding("llama model lock poisoned".into()))?
            .embeddings(texts, params)
            .map_err(|error| EngineError::Embedding(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dot(left: &[f32], right: &[f32]) -> f32 {
        left.iter().zip(right).map(|(a, b)| a * b).sum()
    }

    #[test]
    fn shared_tokens_are_more_similar() {
        let embedder = DeterministicEmbedder::new(128).unwrap();
        let vectors = embedder
            .embed(&[
                "parse rust source code".into(),
                "parse source files".into(),
                "banana orchard weather".into(),
            ])
            .unwrap();
        assert!(dot(&vectors[0], &vectors[1]) > dot(&vectors[0], &vectors[2]));
    }

    #[test]
    fn vectors_are_normalized() {
        let vector = DeterministicEmbedder::new(64)
            .unwrap()
            .embed(&["one two three".into()])
            .unwrap()
            .remove(0);
        assert!((dot(&vector, &vector) - 1.0).abs() < 0.0001);
    }

    #[cfg(feature = "llama-cpp")]
    #[test]
    #[ignore = "requires ZVEC_GREP_TEST_MODEL to point to an embedding GGUF"]
    fn llama_minilm_runtime_smoke_test() {
        let path = std::env::var_os("ZVEC_GREP_TEST_MODEL")
            .expect("set ZVEC_GREP_TEST_MODEL to an embedding GGUF");
        let embedder = LlamaEmbedder::load(LlamaEmbeddingConfig::new(path)).unwrap();
        assert_eq!(embedder.schema().dimension, 384);
        let vectors = embedder
            .embed(&[
                "parse rust source code".into(),
                "parse source files".into(),
                "banana orchard weather".into(),
            ])
            .unwrap();
        assert_eq!(vectors.len(), 3);
        assert!(dot(&vectors[0], &vectors[1]) > dot(&vectors[0], &vectors[2]));
    }
}
