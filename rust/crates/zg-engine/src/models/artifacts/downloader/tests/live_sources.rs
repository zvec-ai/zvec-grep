//! Explicit public-network checks: each downloads the complete pinned Jina model
//! (~164 MB) into a fresh temporary cache. Normal HTTP/proxy configuration applies;
//! neither an existing model cache nor an ONNX runtime is required.

use std::{path::Path, time::Duration};

use super::{
    ArtifactHttpClient, ArtifactSource, ArtifactSourceKind, Manifest,
    ModelDownloadProgressReporter, ResolveArtifacts, async_fs, download_source_snapshot,
    has_valid_complete_marker, partial_files,
};
use crate::models::catalog::{EmbeddingCatalogEntry, get_embedding_model_catalog_entry};

const MODEL_REFERENCE: &str = "local/jina-embeddings-v2-base-code";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(10);

#[tokio::test]
#[ignore = "requires network access to Hugging Face and its CDN; downloads ~164 MB"]
async fn downloads_pinned_jina_from_hugging_face() {
    download_pinned_jina_from(ArtifactSourceKind::HuggingFace).await;
}

#[tokio::test]
#[ignore = "requires network access to ModelScope and its CDN; downloads ~164 MB"]
async fn downloads_pinned_jina_from_model_scope() {
    download_pinned_jina_from(ArtifactSourceKind::ModelScope).await;
}

async fn download_pinned_jina_from(kind: ArtifactSourceKind) {
    let config = get_embedding_model_catalog_entry(MODEL_REFERENCE)
        .and_then(EmbeddingCatalogEntry::transformers_config)
        .expect("catalog contains the pinned Jina transformer model");
    let artifacts = config.download.artifacts;
    assert!(
        artifacts.iter().any(|artifact| {
            Path::new(artifact.path)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("onnx"))
                && artifact.size > 100_000_000
        }),
        "exercise the full model/CDN download, including its ONNX weights"
    );
    let root = tempfile::tempdir().expect("isolated live download cache");
    let reporter = ModelDownloadProgressReporter::new(
        config.reference,
        None,
        artifacts.iter().map(|artifact| artifact.path.to_owned()),
    );
    let request = ResolveArtifacts {
        model: config.reference,
        sources: [
            ArtifactSource::hugging_face(
                config.download.hugging_face,
                root.path().join("huggingface"),
            ),
            ArtifactSource::model_scope(
                config.download.model_scope,
                root.path().join("modelscope"),
            ),
        ],
        artifacts,
        reporter: &reporter,
        signal: None,
    };
    let source_index = match kind {
        ArtifactSourceKind::HuggingFace => 0,
        ArtifactSourceKind::ModelScope => 1,
    };
    let source = &request.sources[source_index];
    let unused_source = &request.sources[1 - source_index];
    assert_eq!(source.kind, kind);
    assert!(source.base_url.is_none(), "use the real source endpoint");
    assert!(
        !source.cache_directory.exists(),
        "every live test starts with an empty source cache"
    );
    let manifest = Manifest::new(config.reference, source, artifacts).expect("live manifest");
    let client = ArtifactHttpClient::new();

    // Calling one source directly prevents a healthy mirror from hiding failures.
    // Production verifies every artifact's pinned SHA-256 and size before publishing.
    tokio::time::timeout(
        DOWNLOAD_TIMEOUT,
        download_source_snapshot(&client, &request, source, &manifest),
    )
    .await
    .unwrap_or_else(|error| panic!("{} download exceeded 10 minutes: {error}", kind.label()))
    .unwrap_or_else(|error| panic!("{} pinned Jina download failed: {error}", kind.label()));

    assert!(
        has_valid_complete_marker(source, artifacts, &manifest)
            .await
            .expect("inspect downloaded completion marker"),
        "{} snapshot must be complete",
        kind.label()
    );
    for artifact in artifacts {
        let path = source
            .local_path(artifact)
            .expect("downloaded artifact path");
        let metadata = async_fs::metadata(&path)
            .await
            .unwrap_or_else(|error| panic!("inspect {}: {error}", path.display()));
        assert!(metadata.is_file(), "{} must be a file", path.display());
        assert_eq!(metadata.len(), artifact.size, "{} size", path.display());
    }
    assert!(
        partial_files(root.path()).is_empty(),
        "no partial downloads"
    );
    assert!(
        !unused_source.cache_directory.exists(),
        "the other source must never be downloaded as fallback"
    );
}
