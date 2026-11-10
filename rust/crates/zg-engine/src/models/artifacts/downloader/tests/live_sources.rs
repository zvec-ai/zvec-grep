//! Explicit public-network checks for every catalog model with local artifacts.
//! Each configured source downloads into fresh per-model temporary caches. Normal
//! HTTP/proxy configuration applies; no existing cache or model runtime is needed.

use std::{collections::HashSet, panic::AssertUnwindSafe, path::Path, time::Duration};

use futures_util::FutureExt;

use super::{
    ArtifactHttpClient, ArtifactSource, ArtifactSourceKind, Manifest,
    ModelDownloadProgressReporter, ResolveArtifacts, async_fs, download_source_snapshot,
    has_valid_complete_marker, partial_files, validate_request,
};
use crate::models::{
    artifacts::ArtifactDownloadConfig,
    catalog::{EmbeddingCatalogEntry, list_embedding_models},
};

const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(10);

#[tokio::test]
#[ignore = "requires Hugging Face/CDN access; downloads every local model configured for this source"]
async fn downloads_all_local_models_from_hugging_face() {
    download_all_local_models_from(ArtifactSourceKind::HuggingFace).await;
}

#[tokio::test]
#[ignore = "requires ModelScope/CDN access; downloads every local model configured for this source"]
async fn downloads_all_local_models_from_model_scope() {
    download_all_local_models_from(ArtifactSourceKind::ModelScope).await;
}

fn local_models() -> Vec<(&'static str, &'static ArtifactDownloadConfig)> {
    list_embedding_models()
        .into_iter()
        .filter_map(|entry| {
            let download = match entry {
                EmbeddingCatalogEntry::LlamaCpp(config) => config.download,
                EmbeddingCatalogEntry::Transformers(config) => config.download,
                EmbeddingCatalogEntry::Model2Vec(config) => config.download,
                EmbeddingCatalogEntry::Bekko(config) => config.download,
                // Qwen API entries have no downloadable local artifacts.
                EmbeddingCatalogEntry::Qwen(_) => return None,
            };
            Some((entry.reference(), download))
        })
        .collect()
}

fn local_models_for(
    kind: ArtifactSourceKind,
) -> Vec<(&'static str, &'static ArtifactDownloadConfig)> {
    local_models()
        .into_iter()
        .filter(|(_, download)| {
            kind == ArtifactSourceKind::HuggingFace || download.model_scope.is_some()
        })
        .collect()
}

async fn download_all_local_models_from(kind: ArtifactSourceKind) {
    let models = local_models_for(kind);
    assert!(!models.is_empty(), "catalog must contain local models");
    // Allow an explicit local recheck without downloading already-passing models.
    // Keep the emitted catalog complete so CI's coverage check rejects partial runs.
    let selected_model = std::env::var("ZVEC_GREP_TEST_DOWNLOAD_MODEL").ok();
    if let Some(selected) = &selected_model {
        assert!(
            models.iter().any(|(model, _)| model == selected),
            "unknown local model: {selected}"
        );
    }
    println!(
        "LIVE_SOURCE_CATALOG={}",
        serde_json::json!({
            "source": kind.label(),
            "models": models.iter().map(|(model, _)| *model).collect::<Vec<_>>(),
        })
    );
    let mut failures = Vec::new();
    for (model, download) in models {
        if selected_model
            .as_ref()
            .is_some_and(|selected| selected != model)
        {
            continue;
        }
        // Isolate failures, including panics, so every catalog entry is attempted.
        let result = AssertUnwindSafe(tokio::time::timeout(
            DOWNLOAD_TIMEOUT,
            download_local_model_from(model, download, kind),
        ))
        .catch_unwind()
        .await;
        let result = match result {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("download and verification exceeded 10 minutes".to_owned()),
            Err(_) => Err("live download check panicked".to_owned()),
        };
        let mut record = serde_json::json!({
            "model": model,
            "source": kind.label(),
            // Manifest totals, including when an incomplete download fails.
            "files": download.artifacts.len(),
            "bytes": download.artifacts.iter().map(|artifact| artifact.size).sum::<u64>(),
            "status": if result.is_ok() { "passed" } else { "failed" },
        });
        if let Err(error) = result {
            failures.push(format!("{model}: {error}"));
            record["error"] = error.into();
        }
        println!("LIVE_SOURCE_RESULT={record}");
    }
    assert!(
        failures.is_empty(),
        "{} failed for {} local models:\n{}",
        kind.label(),
        failures.len(),
        failures.join("\n")
    );
}

fn request<'a>(
    model: &'static str,
    download: &'static ArtifactDownloadConfig,
    root: &Path,
    reporter: &'a ModelDownloadProgressReporter,
) -> ResolveArtifacts<'a> {
    ResolveArtifacts {
        model,
        sources: std::iter::once(ArtifactSource::hugging_face(
            download.hugging_face,
            root.join("huggingface"),
        ))
        .chain(
            download
                .model_scope
                .map(|source| ArtifactSource::model_scope(source, root.join("modelscope"))),
        )
        .collect(),
        artifacts: download.artifacts,
        reporter,
        signal: None,
    }
}

async fn download_local_model_from(
    model: &'static str,
    download: &'static ArtifactDownloadConfig,
    kind: ArtifactSourceKind,
) -> Result<(), String> {
    let root = tempfile::tempdir().map_err(|error| format!("create empty cache: {error}"))?;
    let artifacts = download.artifacts;
    let reporter = ModelDownloadProgressReporter::new(
        model,
        None,
        artifacts.iter().map(|artifact| artifact.path.to_owned()),
    );
    let request = request(model, download, root.path(), &reporter);
    validate_request(&request).map_err(|error| error.to_string())?;
    let source = request
        .sources
        .iter()
        .find(|source| source.kind == kind)
        .ok_or_else(|| format!("{} is not configured for {model}", kind.label()))?;
    if source.kind != kind || source.base_url.is_some() || source.cache_directory.exists() {
        return Err("live source must use its real endpoint and an empty cache".to_owned());
    }
    let manifest = Manifest::new(model, source, artifacts).map_err(|error| error.to_string())?;
    let client = ArtifactHttpClient::new();

    // Calling one source directly prevents a healthy mirror from hiding failures.
    // Production verifies every artifact's pinned SHA-256 and size before publishing.
    let downloaded = download_source_snapshot(&client, &request, source, &manifest)
        .await
        .map_err(|error| error.to_string());
    if request
        .sources
        .iter()
        .any(|other| other.kind != kind && other.cache_directory.exists())
    {
        return Err("the other source was downloaded as fallback".to_owned());
    }
    if !partial_files(root.path()).is_empty() {
        return Err(format!(
            "partial downloads remain; download result: {downloaded:?}"
        ));
    }
    downloaded?;

    if !has_valid_complete_marker(source, artifacts, &manifest)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err("downloaded snapshot has no valid completion marker".to_owned());
    }
    for artifact in artifacts {
        let path = source
            .local_path(artifact)
            .map_err(|error| error.to_string())?;
        let metadata = async_fs::metadata(&path)
            .await
            .map_err(|error| format!("inspect {}: {error}", artifact.path))?;
        if !metadata.is_file() || metadata.len() != artifact.size {
            return Err(format!("{} is not a complete artifact", artifact.path));
        }
    }
    Ok(())
}

#[test]
fn live_source_catalog_includes_exactly_the_models_with_that_source() {
    let hugging_face = local_models_for(ArtifactSourceKind::HuggingFace);
    let model_scope = local_models_for(ArtifactSourceKind::ModelScope);
    for (model, download) in local_models() {
        assert!(
            hugging_face
                .iter()
                .any(|(reference, _)| *reference == model)
        );
        assert_eq!(
            model_scope.iter().any(|(reference, _)| *reference == model),
            download.model_scope.is_some(),
            "{model} source availability"
        );
    }
    assert!(
        hugging_face
            .iter()
            .any(|(model, _)| *model == "local/bekko-256-int8")
    );
    assert!(
        !model_scope
            .iter()
            .any(|(model, _)| *model == "local/bekko-256-int8")
    );
}

#[test]
fn live_source_catalog_covers_every_local_model_and_pinned_weights() {
    let models = local_models();
    let references = models
        .iter()
        .map(|(model, _)| *model)
        .collect::<HashSet<_>>();
    assert!(!models.is_empty());
    assert_eq!(references.len(), models.len(), "unique local model entries");
    for entry in list_embedding_models() {
        let reference = entry.reference();
        if matches!(entry, EmbeddingCatalogEntry::Qwen(_)) {
            assert!(
                !references.contains(reference),
                "API model {reference} is excluded"
            );
            continue;
        }
        let (_, download) = models
            .iter()
            .find(|(model, _)| *model == reference)
            .unwrap_or_else(|| panic!("missing local model {reference}"));
        let root = tempfile::tempdir().expect("isolated catalog validation cache");
        let reporter = ModelDownloadProgressReporter::new(reference, None, []);
        let request = request(reference, download, root.path(), &reporter);
        validate_request(&request)
            .unwrap_or_else(|error| panic!("invalid download metadata for {reference}: {error}"));
        for source in &request.sources {
            assert_eq!(
                source.revision.len(),
                40,
                "{reference} must pin a source commit"
            );
            assert!(source.revision.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
        assert!(download.artifacts.iter().all(|artifact| artifact.size > 0));
        let weight_bytes = match entry {
            EmbeddingCatalogEntry::LlamaCpp(_) => download
                .artifacts
                .iter()
                .filter(|artifact| has_extension(artifact.path, "gguf"))
                .map(|artifact| artifact.size)
                .sum::<u64>(),
            EmbeddingCatalogEntry::Transformers(_) | EmbeddingCatalogEntry::Bekko(_) => {
                assert!(
                    download
                        .artifacts
                        .iter()
                        .any(|artifact| has_extension(artifact.path, "onnx"))
                );
                download
                    .artifacts
                    .iter()
                    .filter(|artifact| {
                        has_extension(artifact.path, "onnx")
                            || has_extension(artifact.path, "onnx_data")
                    })
                    .map(|artifact| artifact.size)
                    .sum::<u64>()
            }
            EmbeddingCatalogEntry::Model2Vec(config) => {
                assert!(
                    download
                        .artifacts
                        .iter()
                        .any(|artifact| artifact.path == config.tokenizer_file)
                );
                download
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.path == config.model_file)
                    .expect("Model2Vec weights are included")
                    .size
            }
            EmbeddingCatalogEntry::Qwen(_) => unreachable!("API models were excluded"),
        };
        assert!(
            weight_bytes > 1_000_000,
            "{reference} must include full model weights"
        );
    }
}

fn has_extension(path: &str, extension: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}
