use std::collections::{BTreeMap, BTreeSet};

use crate::{
    EngineError,
    api::index::{
        IndexOptions,
        options::{Device, EmbeddingModelSpec},
    },
    domain::{
        ContentKind, EmbeddingModelInfo, FTS_CONFIG, IndexDescriptor, Workspace, model::ModelConfig,
    },
    models::{EmbeddingCatalogEntry, ModelError},
};

pub(super) struct EmbeddingPlan {
    pub proposed: IndexDescriptor,
    pub requests: BTreeMap<String, IndexOptions>,
    pub active: BTreeSet<String>,
}

impl EmbeddingPlan {
    pub(super) fn new(
        existing: Option<&Workspace>,
        options: &IndexOptions,
        default_reference: &str,
    ) -> Result<Self, EngineError> {
        let previous = existing.and_then(|w| w.index().descriptor());
        let mut cleared = BTreeSet::new();
        for &kind in &options.clear_embedding_routes {
            if !cleared.insert(kind) || options.embedding_routes.contains_key(&kind) {
                return Err(EngineError::invalid_argument(format!(
                    "duplicate embedding update for {}",
                    kind.as_str()
                )));
            }
        }
        let mut specs = BTreeMap::new();
        let mut default_spec = options
            .embedding
            .clone()
            .unwrap_or_else(|| bare_spec(default_reference.to_owned()));
        if let Some(endpoint) = &options.endpoint {
            default_spec.endpoint = Some(endpoint.clone());
        }
        specs.insert(default_reference.to_owned(), default_spec);
        let mut overrides = BTreeMap::new();
        for (&kind, spec) in &options.embedding_routes {
            if let Some(stored) = specs.get_mut(&spec.reference) {
                merge_spec(stored, spec)?;
            } else {
                specs.insert(spec.reference.clone(), spec.clone());
            }
            overrides.insert(kind, spec.reference.clone());
        }
        if let Some(previous) = previous {
            for (&kind, saved) in previous.embedding_routes() {
                if !cleared.contains(&kind) && !options.embedding_routes.contains_key(&kind) {
                    overrides.insert(kind, saved.model().reference());
                }
            }
        }
        let proposed =
            proposed_descriptor(previous, options, default_reference, overrides, &specs)?;
        let mut requests = BTreeMap::new();
        for model in proposed.embeddings() {
            let reference = model.model().reference();
            let spec = specs
                .remove(&reference)
                .unwrap_or_else(|| bare_spec(reference.clone()));
            if reference.starts_with("local/") && spec.endpoint.is_some() {
                return Err(EngineError::invalid_argument(
                    "endpoint requires a remote model",
                ));
            }
            if spec.revision.is_some() {
                return Err(EngineError::unsupported(
                    "embedding revision overrides are not supported by the catalog-backed runtime",
                ));
            }
            let mut request = options.clone();
            request.embedding = Some(spec);
            request.embedding_routes.clear();
            request.clear_embedding_routes.clear();
            if reference != default_reference {
                request.api_key = None;
                request.runtime_api_key = None;
                request.endpoint = None;
            }
            if let Some(http) = proposed.embedding_configs().get(&reference) {
                request.endpoint.clone_from(&http.endpoint);
            }
            if !reference.starts_with("local/") {
                request.device = None;
                request.runtime_device = None;
            }
            requests.insert(reference, request);
        }
        let active = proposed
            .content_indexes()
            .into_iter()
            .map(|index| index.embedding.model().reference())
            .collect::<BTreeSet<_>>();
        if options.device.is_some() && !active.iter().any(|model| model.starts_with("local/")) {
            return Err(EngineError::invalid_argument(
                "device requires a local model",
            ));
        }
        Ok(Self {
            proposed,
            requests,
            active,
        })
    }
}

fn bare_spec(reference: String) -> EmbeddingModelSpec {
    EmbeddingModelSpec {
        reference,
        revision: None,
        cache_dir: None,
        endpoint: None,
        device: Device::Auto,
    }
}

/// Routes sharing a runtime can specify complementary settings. Reject two
/// explicit values for the same property before acquiring either model.
fn merge_spec(
    stored: &mut EmbeddingModelSpec,
    other: &EmbeddingModelSpec,
) -> Result<(), EngineError> {
    fn merge<T: Clone + Eq>(current: &mut Option<T>, next: Option<&T>) -> bool {
        match (current.as_ref(), next) {
            (Some(first), Some(second)) => first == second,
            (None, Some(value)) => {
                *current = Some(value.clone());
                true
            }
            _ => true,
        }
    }
    let mut device = (stored.device != Device::Auto).then_some(stored.device);
    if !merge(&mut stored.endpoint, other.endpoint.as_ref())
        || !merge(&mut stored.revision, other.revision.as_ref())
        || !merge(
            &mut device,
            (other.device != Device::Auto).then_some(&other.device),
        )
    {
        return Err(EngineError::invalid_argument(format!(
            "conflicting settings for embedding model {}",
            stored.reference
        )));
    }
    stored.device = device.unwrap_or(Device::Auto);
    Ok(())
}

/// Build the schema before runtime acquisition. Even a fully overridden HTTP
/// default retains its definition so the workspace can later restore inheritance.
fn proposed_descriptor(
    previous: Option<&IndexDescriptor>,
    options: &IndexOptions,
    default_reference: &str,
    overrides: BTreeMap<ContentKind, String>,
    specs: &BTreeMap<String, EmbeddingModelSpec>,
) -> Result<IndexDescriptor, EngineError> {
    let global = crate::config::read()?;
    let mut configs = BTreeMap::new();
    let mut resolve = |reference: &str| -> Result<EmbeddingModelInfo, EngineError> {
        let saved = previous.and_then(|index| index.embedding_configs().get(reference));
        let endpoint = specs
            .get(reference)
            .and_then(|spec| spec.endpoint.as_deref());
        if let Some(runtime) =
            crate::config::resolve_http_model(&global, reference, saved, endpoint, options.rebuild)?
        {
            let http = runtime.http.expect("resolved HTTP definition");
            let model = http.model_info(reference)?;
            configs.insert(
                reference.to_owned(),
                ModelConfig {
                    http: Some(http),
                    endpoint: runtime.endpoint,
                    ..Default::default()
                },
            );
            return Ok(model);
        }
        EmbeddingCatalogEntry::try_from(reference)
            .map(EmbeddingCatalogEntry::embedding_info)
            .map_err(ModelError::into_engine_error)
    };
    let default_model = resolve(default_reference)?;
    let mut routes = BTreeMap::new();
    for (kind, reference) in overrides {
        let model = resolve(&reference)?;
        if !model.model().supports_content(kind) {
            return Err(EngineError::unsupported(format!(
                "model {reference} cannot embed {}",
                kind.as_str()
            )));
        }
        routes.insert(kind, model);
    }
    let graph_indexing = options
        .graph_indexing
        .unwrap_or_else(|| previous.is_some_and(IndexDescriptor::graph_indexing));
    IndexDescriptor::new(default_model, routes, configs, graph_indexing, FTS_CONFIG)
}

/// Concrete destinations shared by indexing and remote authorization preflight.
pub(crate) fn embedding_requests(
    existing: Option<&Workspace>,
    options: &IndexOptions,
    default_reference: &str,
) -> Result<(IndexDescriptor, BTreeMap<String, IndexOptions>), EngineError> {
    let mut options = options.clone();
    options.normalize_model_cache()?;
    let mut plan = EmbeddingPlan::new(existing, &options, default_reference)?;
    plan.requests
        .retain(|reference, _| plan.active.contains(reference));
    Ok((plan.proposed, plan.requests))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ContentKind;

    #[test]
    fn shared_remote_runtime_merges_settings_and_rejects_conflicts() {
        let reference = "qwen/text-embedding-v4".to_owned();
        let code = bare_spec(reference.clone());
        let mut options = IndexOptions {
            embedding: Some(bare_spec(reference.clone())),
            endpoint: Some("https://one.test/embeddings".into()),
            api_key: Some("default-key".into()),
            embedding_routes: BTreeMap::from([(ContentKind::Code, code)]),
            ..Default::default()
        };
        let (_, requests) = embedding_requests(None, &options, &reference).expect("shared runtime");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[&reference].endpoint, options.endpoint);
        options
            .embedding_routes
            .get_mut(&ContentKind::Code)
            .expect("code override")
            .endpoint = Some("https://two.test/embeddings".into());
        assert!(
            embedding_requests(None, &options, &reference)
                .expect_err("conflicting destinations")
                .message()
                .contains("conflicting")
        );
    }

    #[test]
    fn override_credentials_and_destinations_are_scoped_to_the_model() {
        let default = "qwen/text-embedding-v4".to_owned();
        let other = "qwen/qwen3-vl-embedding".to_owned();
        let options = IndexOptions {
            embedding: Some(bare_spec(default.clone())),
            endpoint: Some("https://default.test/embeddings".into()),
            api_key: Some("saved-key".into()),
            runtime_api_key: Some("runtime-key".into()),
            embedding_routes: BTreeMap::from([(ContentKind::Code, bare_spec(other.clone()))]),
            ..Default::default()
        };
        let (_, requests) = embedding_requests(None, &options, &default).expect("distinct models");
        let request = &requests[&other];
        assert!(
            request.api_key.is_none()
                && request.runtime_api_key.is_none()
                && request.endpoint.is_none()
        );
    }

    #[test]
    fn workspace_device_applies_to_active_local_models_in_either_route_order() {
        let local = "local/bekko-256-int8";
        let remote = "qwen/text-embedding-v4";
        for (default, pin) in [(local, remote), (remote, local)] {
            let options = IndexOptions {
                embedding: Some(bare_spec(default.into())),
                embedding_routes: BTreeMap::from([(ContentKind::Code, bare_spec(pin.into()))]),
                device: Some(Device::Cpu),
                runtime_device: Some(Device::Auto),
                ..Default::default()
            };
            let (_, requests) = embedding_requests(None, &options, default).expect("mixed routes");
            assert_eq!(requests[local].device, Some(Device::Cpu));
            assert_eq!(requests[local].runtime_device, Some(Device::Auto));
            assert_eq!(requests[remote].device, None);
            assert_eq!(requests[remote].runtime_device, None);
        }
        for default in [local, remote] {
            let options = IndexOptions {
                embedding_routes: [ContentKind::Text, ContentKind::Code]
                    .into_iter()
                    .map(|kind| (kind, bare_spec(remote.into())))
                    .collect(),
                device: Some(Device::Cpu),
                ..Default::default()
            };
            let error = embedding_requests(None, &options, default)
                .expect_err("all active routes are remote");
            assert!(error.message().contains("local model"));
        }
    }

    #[test]
    fn duplicate_route_updates_fail_early() {
        let default = "local/bekko-256-int8".to_owned();
        for clear in [
            vec![ContentKind::Code],
            vec![ContentKind::Text, ContentKind::Text],
        ] {
            let options = IndexOptions {
                embedding_routes: BTreeMap::from([(ContentKind::Code, bare_spec(default.clone()))]),
                clear_embedding_routes: clear,
                ..Default::default()
            };
            assert!(embedding_requests(None, &options, &default).is_err());
        }
    }

    #[test]
    fn image_capability_determines_active_runtimes() {
        let local = "local/bekko-256-int8";
        let multimodal = "qwen/qwen3-vl-embedding";
        let mut options = IndexOptions {
            embedding_routes: [ContentKind::Text, ContentKind::Code]
                .into_iter()
                .map(|kind| (kind, bare_spec(local.into())))
                .collect(),
            ..Default::default()
        };
        let (_, active) = embedding_requests(None, &options, multimodal).expect("image default");
        assert!(active.contains_key(multimodal));
        assert!(active.contains_key(local));

        options
            .embedding_routes
            .insert(ContentKind::Image, bare_spec(local.into()));
        let error = embedding_requests(None, &options, multimodal)
            .expect_err("text-only model cannot handle the image override");
        assert!(error.message().contains("cannot embed image"));
    }
}
