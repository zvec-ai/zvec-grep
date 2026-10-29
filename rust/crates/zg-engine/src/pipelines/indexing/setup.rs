use crate::{
    EngineError,
    api::index::setup::{EmbeddingChoice, IndexSetup, IndexSetupOptions},
    config,
    models::{
        EmbeddingCatalogEntry, ResolveEmbeddingReferenceOptions, list_embedding_models,
        model_is_cached, resolve_embedding_reference,
    },
};

pub(crate) async fn index_setup(options: IndexSetupOptions) -> Result<IndexSetup, EngineError> {
    let config = config::read()?;
    let default_model = resolve_embedding_reference(ResolveEmbeddingReferenceOptions {
        explicit: options.preferred_model,
        global_default: config::string(&config, &["defaults", "embedding"]),
        ..Default::default()
    })
    .map_err(crate::models::ModelError::into_engine_error)?;
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
    sort_choices(&mut models, default_model.as_deref());
    Ok(IndexSetup {
        models,
        default_model,
    })
}

fn sort_choices(models: &mut [EmbeddingChoice], default: Option<&str>) {
    models.sort_by_key(|model| {
        (
            !model.local,
            if model.local {
                !model.downloaded
            } else {
                !model.configured
            },
            Some(model.reference.as_str()) != default,
            model.reference != "local/potion-code-16m-v2",
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
    fn each_tab_prioritizes_cached_or_configured_models_before_defaults() {
        let mut models = vec![
            choice("remote/default", false, false, false),
            choice("local/default", true, false, false),
            choice("local/cached", true, true, false),
            choice("remote/configured", false, false, true),
        ];
        sort_choices(&mut models, Some("local/default"));
        assert_eq!(
            models
                .iter()
                .map(|m| m.reference.as_str())
                .collect::<Vec<_>>(),
            [
                "local/cached",
                "local/default",
                "remote/configured",
                "remote/default"
            ]
        );
        assert!(!models[2].available());
        assert!(models[0].available());
    }
}
