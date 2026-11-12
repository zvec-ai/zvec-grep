use crate::{
    EngineError,
    api::index::{
        IndexOptions,
        options::EmbeddingModelSpec,
        setup::{EmbeddingChoice, IndexSetup, IndexSetupOptions},
    },
    config,
    models::{EmbeddingCatalogEntry, list_embedding_models, model_is_cached},
};

use super::embedding_policy;

pub(crate) async fn index_setup(mut options: IndexSetupOptions) -> Result<IndexSetup, EngineError> {
    let config = config::read()?;
    let request = IndexOptions {
        embedding: options.preferred_model.map(|reference| EmbeddingModelSpec {
            reference,
            revision: None,
            cache_dir: None,
            endpoint: None,
            device: crate::domain::model::Device::Auto,
        }),
        embedding_environment: options.embedding_environment,
        signal: options.signal,
        ..Default::default()
    };
    let selection = if let Some(root) = options.root {
        let root = crate::utils::resolve_directory(Some(&root))?;
        options.scan.normalize_ignore_paths(&root)?;
        let scan = options.scan.apply(&crate::domain::ScanRules::default())?;
        Some(embedding_policy::select(&root, &scan, None, &request).await?)
    } else {
        embedding_policy::configured_selection(None, &request)?
    };
    let default_model = selection.as_ref().map(|selection| selection.model.clone());
    if default_model.as_deref().is_some_and(|reference| {
        crate::models::get_embedding_model_catalog_entry(reference).is_none()
    }) {
        return Err(EngineError::invalid_argument(
            "Unsupported default embedding model; run zg --help models",
        ));
    }
    let cache = config::model_cache(&config, options.model_cache, None)
        .map(std::path::absolute)
        .transpose()
        .map_err(|error| EngineError::from_io("failed to resolve model cache directory", &error))?;
    let mut models = Vec::new();
    for entry in list_embedding_models() {
        let reference = entry.reference();
        let local = !matches!(entry, EmbeddingCatalogEntry::Qwen(_));
        let download = match entry {
            EmbeddingCatalogEntry::LlamaCpp(model) => Some(model.download),
            EmbeddingCatalogEntry::Model2Vec(model) => Some(model.download),
            EmbeddingCatalogEntry::Transformers(model) => Some(model.download),
            EmbeddingCatalogEntry::Bekko(model) => Some(model.download),
            EmbeddingCatalogEntry::Qwen(_) => None,
        };
        let provider = reference.split('/').next().unwrap_or_default();
        models.push(EmbeddingChoice {
            reference: reference.to_owned(),
            local,
            downloaded: local && model_is_cached(entry, cache.as_deref()).await,
            configured: config["models"].get(reference).is_some()
                || config["defaults"]["embedding"].as_str() == Some(reference),
            credential_available: local
                || options.credential_available
                || config::string(&config, &["providers", provider, "apiKey"])
                    .is_some_and(|key| !key.trim().is_empty())
                || super::service::environment_api_key().is_some(),
            download_bytes: download
                .map(|download| download.artifacts.iter().map(|a| a.size).sum()),
        });
    }
    let recommended = selection
        .as_ref()
        .and_then(|selection| selection.analysis.as_ref())
        .map_or(embedding_policy::CODE_MODEL, |analysis| {
            analysis.recommended_model.as_str()
        });
    sort_choices(&mut models, default_model.as_deref(), recommended);
    Ok(IndexSetup {
        models,
        default_model,
        selection,
    })
}

fn sort_choices(models: &mut [EmbeddingChoice], default: Option<&str>, recommended: &str) {
    models.sort_by_key(|model| {
        (
            !model.local,
            Some(model.reference.as_str()) != default,
            model.reference != recommended,
            if model.local {
                !model.downloaded
            } else {
                !model.configured
            },
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(reference: &str, local: bool, downloaded: bool, configured: bool) -> EmbeddingChoice {
        EmbeddingChoice {
            reference: reference.into(),
            local,
            downloaded,
            configured,
            credential_available: local,
            download_bytes: None,
        }
    }

    #[test]
    fn each_tab_prioritizes_defaults_and_recommendations_before_cached_models() {
        let mut models = vec![
            choice("remote/default", false, false, false),
            choice("local/default", true, false, false),
            choice("local/cached", true, true, false),
            choice("local/recommended", true, false, false),
            choice("remote/configured", false, false, true),
        ];
        sort_choices(&mut models, Some("local/default"), "local/recommended");
        assert_eq!(
            models
                .iter()
                .map(|m| m.reference.as_str())
                .collect::<Vec<_>>(),
            [
                "local/default",
                "local/recommended",
                "local/cached",
                "remote/configured",
                "remote/default"
            ]
        );
        assert!(!models[3].available());
        assert!(models[0].available());
    }
}
