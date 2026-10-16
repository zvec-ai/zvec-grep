//! Read-only inspection of the same snapshots used by model preparation.

use std::path::{Path, PathBuf};

use crate::models::catalog::EmbeddingCatalogEntry;

use super::{
    ArtifactSource,
    manifest::{Manifest, validate_snapshot},
};

pub(crate) fn default_model_cache() -> PathBuf {
    std::env::var_os("ZVEC_GREP_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|home| PathBuf::from(home).join(".zvec-grep"))
        })
        .unwrap_or_else(|| PathBuf::from(".zvec-grep"))
        .join("models")
}

pub(crate) async fn model_is_cached(entry: EmbeddingCatalogEntry, cache: &Path) -> bool {
    let (download, sources) = match entry {
        EmbeddingCatalogEntry::Qwen(_) => return false,
        EmbeddingCatalogEntry::LlamaCpp(model) => (
            model.download,
            [
                ArtifactSource::hugging_face(model.download.hugging_face, cache.to_path_buf())
                    .with_local_path(model.download.artifacts[0].path, model.cache_file),
                ArtifactSource::model_scope(
                    model.download.model_scope,
                    cache
                        .join("modelscope")
                        .join("llama-cpp")
                        .join(model.download.model_scope.repo.replace('/', "--"))
                        .join(model.download.model_scope.revision),
                ),
            ],
        ),
        EmbeddingCatalogEntry::Transformers(model) => (
            model.download,
            [
                ArtifactSource::hugging_face(
                    model.download.hugging_face,
                    cache
                        .join(model.download.hugging_face.repo)
                        .join(model.download.hugging_face.revision),
                ),
                ArtifactSource::model_scope(
                    model.download.model_scope,
                    cache
                        .join("modelscope")
                        .join("transformers-js")
                        .join(model.download.model_scope.repo.replace('/', "--"))
                        .join(model.download.model_scope.revision),
                ),
            ],
        ),
        EmbeddingCatalogEntry::Model2Vec(model) => {
            let Some(model_file) = Path::new(model.model_file).file_name() else {
                return false;
            };
            (
                model.download,
                [
                    ArtifactSource::hugging_face(
                        model.download.hugging_face,
                        cache
                            .join("model2vec")
                            .join(model.download.hugging_face.repo.replace('/', "--"))
                            .join(model.download.hugging_face.revision),
                    )
                    .with_local_path(model.model_file, model_file)
                    .with_local_path(model.tokenizer_file, "tokenizer/tokenizer.json"),
                    ArtifactSource::model_scope(
                        model.download.model_scope,
                        cache
                            .join("modelscope")
                            .join("model2vec")
                            .join(model.download.model_scope.repo.replace('/', "--"))
                            .join(model.download.model_scope.revision),
                    )
                    .with_local_path(model.model_file, model_file)
                    .with_local_path(model.tokenizer_file, "tokenizer/tokenizer.json"),
                ],
            )
        }
    };
    for source in &sources {
        let Ok(manifest) = Manifest::new(entry.reference(), source, download.artifacts) else {
            continue;
        };
        if validate_snapshot(source, download.artifacts, &manifest, None)
            .await
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        artifacts::{ArtifactConfig, ArtifactDownloadConfig, ArtifactSourceConfig},
        catalog::get_embedding_model_catalog_entry,
    };

    const SOURCE: ArtifactSourceConfig = ArtifactSourceConfig {
        repo: "fixture/model",
        revision: "fixture",
    };
    const DOWNLOAD: ArtifactDownloadConfig = ArtifactDownloadConfig {
        hugging_face: SOURCE,
        model_scope: SOURCE,
        artifacts: &[
            ArtifactConfig {
                path: "model.safetensors",
                size: 3,
                sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            },
            ArtifactConfig {
                path: "tokenizer.json",
                size: 3,
                sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            },
        ],
    };

    #[tokio::test]
    async fn inspection_checks_every_artifact_without_downloading_or_writing_markers() {
        let directory = tempfile::tempdir().expect("isolated cache");
        let cache = directory.path().join("models");
        let EmbeddingCatalogEntry::Model2Vec(mut model) =
            get_embedding_model_catalog_entry("local/potion-code-16m-v2").expect("catalog model")
        else {
            panic!("model2vec");
        };
        model.download = &DOWNLOAD;
        model.model_file = "model.safetensors";
        model.tokenizer_file = "tokenizer.json";
        let entry = EmbeddingCatalogEntry::Model2Vec(model);
        assert!(!model_is_cached(entry, &cache).await);
        assert!(
            !cache.exists(),
            "inspection must not create cache directories"
        );
        let snapshot = cache.join("model2vec/fixture--model/fixture");
        std::fs::create_dir_all(snapshot.join("tokenizer")).expect("snapshot");
        std::fs::write(snapshot.join("model.safetensors"), b"abc").expect("model");
        assert!(
            !model_is_cached(entry, &cache).await,
            "partial downloads are unavailable"
        );
        std::fs::write(snapshot.join("tokenizer/tokenizer.json"), b"abc").expect("tokenizer");
        assert!(model_is_cached(entry, &cache).await);
        assert_eq!(
            std::fs::read_dir(&snapshot).expect("cache entries").count(),
            2,
            "inspection must not write completion markers"
        );
        std::fs::write(snapshot.join("model.safetensors"), b"xyz").expect("corrupt model");
        assert!(
            !model_is_cached(entry, &cache).await,
            "matching file sizes do not prove integrity"
        );
        let fallback = cache.join("modelscope/model2vec/fixture--model/fixture");
        std::fs::create_dir_all(fallback.join("tokenizer")).expect("fallback snapshot");
        std::fs::write(fallback.join("model.safetensors"), b"abc").expect("fallback model");
        std::fs::write(fallback.join("tokenizer/tokenizer.json"), b"abc")
            .expect("fallback tokenizer");
        assert!(
            model_is_cached(entry, &cache).await,
            "verified fallback snapshots count as downloaded"
        );
    }
}
