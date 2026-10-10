use serde_json::json;

use super::EmbeddingCatalogEntry;
use crate::domain::model::{EmbeddingRetrieval, EmbeddingSpace};
use crate::models::artifacts::ArtifactConfig;

pub(crate) const GEMMA_QUERY_PREFIX: &str = "task: search result | query: ";
pub(crate) const GEMMA_CODE_QUERY_PREFIX: &str = "task: code retrieval | query: ";
pub(crate) const GEMMA_DOCUMENT_PREFIX: &str = "title: none | text: ";
pub(crate) const QWEN3_QUERY_PREFIX: &str =
    "Instruct: Retrieve relevant documents for the given query\nQuery: ";

impl EmbeddingCatalogEntry {
    pub(crate) fn retrieval(self) -> EmbeddingRetrieval {
        match self {
            // Both publishers document a shared text/image retrieval space. Do
            // not infer this capability from the adapter's accepted input kinds.
            Self::EmbeddingGemma2(_) => EmbeddingRetrieval::TextImage,
            Self::Qwen(entry) if entry.model == "qwen3-vl-embedding" => {
                EmbeddingRetrieval::TextImage
            }
            _ => EmbeddingRetrieval::Text,
        }
    }

    pub(crate) fn embedding_space(self, endpoint: Option<&str>) -> EmbeddingSpace {
        let (revision, recipe) = match self {
            Self::EmbeddingGemma2(entry) => (
                Some(entry.source.revision),
                json!({
                    "adapter": "embeddinggemma2-onnx-v1",
                    "artifacts": artifact_checksums(entry.artifacts),
                    "pooling": "graph-masked-mean-projection-512-to-768",
                    "normalization": "graph-l2",
                    "queryPrefix": GEMMA_QUERY_PREFIX,
                    "codeQueryPrefix": GEMMA_CODE_QUERY_PREFIX,
                    "documentPrefix": GEMMA_DOCUMENT_PREFIX,
                    "imageProcessor": "exif-rgb-catmulrom-645120-area-48-multiple-hwc16-pool3-280-tokens-v1",
                    "maxTokens": entry.max_input_tokens,
                    "dimension": entry.dimension,
                }),
            ),
            Self::LlamaCpp(entry) => (
                Some(entry.download.hugging_face.revision),
                json!({
                    "adapter": "llama-cpp-sequence-embedding-v1",
                    "artifacts": artifact_checksums(entry.download.artifacts),
                    "pooling": "gguf-model-default",
                    "normalization": "none",
                    "queryPrefix": if entry.format == "qwen3" { QWEN3_QUERY_PREFIX } else { GEMMA_QUERY_PREFIX },
                    "documentPrefix": if entry.format == "qwen3" { "" } else { GEMMA_DOCUMENT_PREFIX },
                    "maxTokens": entry.context_size,
                    "dimension": entry.dimension,
                }),
            ),
            Self::Transformers(entry) => (
                Some(entry.revision),
                json!({
                    "adapter": "transformers-onnx-v1",
                    "artifacts": artifact_checksums(entry.download.artifacts),
                    "dtype": entry.dtype,
                    "pooling": entry.pooling,
                    "normalize": entry.normalize,
                    "queryPrefix": entry.query_prefix,
                    "documentPrefix": entry.document_prefix,
                    "maxTokens": entry.max_input_tokens,
                    "dimension": entry.dimension,
                }),
            ),
            Self::Model2Vec(entry) => (
                Some(entry.revision),
                json!({
                    "adapter": "model2vec-mean-token-embedding-v1",
                    "artifacts": artifact_checksums(entry.download.artifacts),
                    "embeddingTensor": entry.embedding_tensor,
                    "normalize": entry.normalize,
                    "queryPrefix": entry.query_prefix,
                    "documentPrefix": entry.document_prefix,
                    "maxTokens": entry.max_input_tokens,
                    "dimension": entry.dimension,
                }),
            ),
            Self::Qwen(entry) => (
                None,
                json!({
                    "adapter": "dashscope-embedding-v1",
                    "service": service_identity(endpoint.unwrap_or(entry.default_endpoint)),
                    "model": entry.model,
                    "fusion": entry.kind == "multimodal",
                    "normalization": "provider-managed",
                    "prefix": "none",
                    "dimension": entry.dimension,
                }),
            ),
        };
        // Bump the adapter/processor recipe when changing encoding behavior.
        // JSON fixes field boundaries; credentials never enter this identity.
        let bytes = serde_json::to_vec(&recipe).expect("catalog recipe is serializable");
        EmbeddingSpace {
            revision: revision.map(str::to_owned),
            encoding_fingerprint: crate::utils::sha256_hex(&bytes),
            code_query_instruction: matches!(self, Self::EmbeddingGemma2(_))
                .then(|| GEMMA_CODE_QUERY_PREFIX.to_owned()),
        }
    }
}

fn artifact_checksums(artifacts: &[ArtifactConfig]) -> Vec<(&str, &str)> {
    artifacts
        .iter()
        .map(|artifact| (artifact.path, artifact.sha256))
        .collect()
}

fn service_identity(endpoint: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(endpoint) else {
        return "invalid-endpoint".to_owned();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::catalog::get_embedding_model_catalog_entry;

    #[test]
    fn catalog_declares_verified_cross_modal_retrieval_separately_from_inputs() {
        for reference in ["local/embeddinggemma-2", "qwen/qwen3-vl-embedding"] {
            let entry = get_embedding_model_catalog_entry(reference).expect("catalog entry");
            assert_eq!(entry.retrieval(), EmbeddingRetrieval::TextImage);
        }
        let text = get_embedding_model_catalog_entry("qwen/text-embedding-v4")
            .expect("text catalog entry");
        assert_eq!(text.retrieval(), EmbeddingRetrieval::Text);
        let EmbeddingCatalogEntry::Qwen(mut unverified) = text else {
            panic!("qwen entry");
        };
        // Merely accepting images cannot advertise cross-modal retrieval.
        unverified.kind = "multimodal";
        let unverified = EmbeddingCatalogEntry::Qwen(unverified);
        assert!(
            unverified
                .model_info()
                .expect("metadata")
                .supports_content(crate::domain::ContentKind::Image)
        );
        assert_eq!(unverified.retrieval(), EmbeddingRetrieval::Text);
    }

    #[test]
    fn persisted_space_changes_with_weights_normalization_and_instructions() {
        let entry =
            get_embedding_model_catalog_entry("local/potion-code-16m-v2").expect("catalog entry");
        let original = entry.embedding_space(None);
        let EmbeddingCatalogEntry::Model2Vec(config) = entry else {
            panic!("model2vec entry");
        };
        let mut changed = config;
        changed.revision = "different-weights";
        assert_ne!(
            original,
            EmbeddingCatalogEntry::Model2Vec(changed).embedding_space(None)
        );
        changed = config;
        changed.normalize = !changed.normalize;
        assert_ne!(
            original,
            EmbeddingCatalogEntry::Model2Vec(changed).embedding_space(None)
        );
        changed = config;
        changed.query_prefix = Some("different task: ");
        assert_ne!(
            original,
            EmbeddingCatalogEntry::Model2Vec(changed).embedding_space(None)
        );
        changed = config;
        changed.document_prefix = Some("different document: ");
        assert_ne!(
            original,
            EmbeddingCatalogEntry::Model2Vec(changed).embedding_space(None)
        );
    }

    #[test]
    fn remote_alias_is_unpinned_and_endpoint_identity_omits_credentials() {
        let entry =
            get_embedding_model_catalog_entry("qwen/qwen3-vl-embedding").expect("catalog entry");
        assert_eq!(entry.embedding_space(None).revision, None);
        assert_ne!(
            entry.embedding_space(Some("https://first.example/embeddings")),
            entry.embedding_space(Some("https://second.example/embeddings")),
        );
        assert_eq!(
            service_identity("https://user:password@example.com/embeddings?api_key=secret#token"),
            "https://example.com/embeddings"
        );
    }
}
