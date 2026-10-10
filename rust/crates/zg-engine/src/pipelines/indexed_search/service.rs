//! Resolve the indexed workspace, refresh it when requested, and execute retrieval.

use std::path::{Path, PathBuf};

use crate::{
    EngineError,
    api::{
        context::{ContextOptions, ContextResult},
        index::IndexOptions,
    },
    domain::{ContentKind, EmbeddingModelInfo, IndexState, Workspace, model::ModelConfig},
    models::{ModelError, ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest},
    pipelines::indexing::service::{
        WorkspaceIndexService, assert_embedding_compatible, environment_api_key, is_indexed,
    },
    storage::{
        IndexStore,
        read_session::{ReadSessionCache, ReadSessionLease},
    },
    workspace::{
        layout::find_nearest_workspace,
        lock::{LockWait, try_home_read},
        manifest::{WorkspaceManifest, read_workspace_manifest},
    },
};

use super::{
    context::{context_from_index, normalize_context_request},
    pipeline::{RequestEmbeddingRuntime, SearchEmbeddingRuntime},
};

pub(crate) async fn context(
    indexing: &WorkspaceIndexService,
    models: &ModelRuntimeManager,
    options: &ContextOptions,
    read_sessions: Option<&ReadSessionCache>,
    refresh_completed: bool,
) -> Result<ContextResult, EngineError> {
    let wait = LockWait::new(options.signal.as_ref(), options.lock_timeout_ms)?;
    wait.check_cancelled()?;
    let requested_root =
        std::path::absolute(options.root.as_deref().unwrap_or_else(|| Path::new(".")))
            .map_err(|error| EngineError::from_io("failed to resolve workspace root", &error))?;
    let request = normalize_context_request(options)?;
    let Some(location) = find_nearest_workspace(&requested_root)? else {
        return Err(workspace_index_unavailable(
            &requested_root,
            "no workspace manifest was found",
        ));
    };
    let initial_manifest = read_workspace_manifest(&location.home)?;
    if let Some(manifest) = initial_manifest
        .as_ref()
        .filter(|manifest| is_indexed(manifest))
    {
        query_targets(&manifest.workspace, options, &request)?;
    }
    if !initial_manifest
        .as_ref()
        .map(|manifest| IndexStore::exists(&manifest.storage_home()))
        .transpose()?
        .unwrap_or(false)
    {
        return Err(workspace_index_unavailable(
            &location.root,
            "index storage is missing",
        ));
    }
    if !refresh_completed
        && options.refresh.map_or(options.auto_update, |policy| {
            policy == crate::api::context::options::RefreshPolicy::Wait
        })
        && indexing.workspace_needs_refresh(&location, &wait).await?
    {
        indexing
            .index(models, refresh_options(options, location.root.clone()))
            .await?;
    }
    let allows_writer = options.refresh.map_or(!options.auto_update, |policy| {
        policy != crate::api::context::options::RefreshPolicy::Wait
    });
    let _lock = loop {
        wait.check_cancelled()?;
        if allows_writer
            && let Some(result) =
                try_writer_context(indexing, models, &location, options, &request).await?
        {
            return Ok(result);
        }
        if let Some(lock) = try_home_read(&location.home, "context")? {
            break lock;
        }
        wait.retry(&location.home, "context").await?;
    };
    let mut manifest = read_workspace_manifest(&location.home)?.ok_or_else(|| {
        workspace_index_unavailable(&location.root, "workspace manifest disappeared")
    })?;
    indexing.reconcile_name(&mut manifest)?;
    if manifest.workspace.index == IndexState::Disabled {
        return Err(EngineError::unsupported(format!(
            "workspace indexing is disabled at {}",
            location.root.display()
        )));
    }
    if !is_indexed(&manifest) {
        return Err(workspace_index_unavailable(
            &location.root,
            "workspace index has not been built",
        ));
    }
    let acquired = query_models(models, &manifest, options, &location.root, &request)?;
    let storage = match read_sessions {
        Some(cache) => cache.acquire(&location.home, &manifest.storage_home())?,
        None => ReadSessionLease::open(&manifest.storage_home())?,
    };
    let result = query_storage(
        &acquired,
        &location.root,
        &manifest,
        storage.storage(),
        options,
        &request,
    )
    .await;
    let close = storage.close();
    let result = result?;
    close?;
    Ok(result)
}

async fn try_writer_context(
    indexing: &WorkspaceIndexService,
    models: &ModelRuntimeManager,
    location: &crate::workspace::layout::WorkspaceIndexLocation,
    options: &ContextOptions,
    request: &super::context::NormalizedContextRequest,
) -> Result<Option<ContextResult>, EngineError> {
    let Some(active) = read_workspace_manifest(&location.home)? else {
        return Ok(None);
    };
    if !is_indexed(&active) {
        return Ok(None);
    }
    let Some(writer) = indexing
        .writers
        .borrow(&location.home, &active.storage_home())
    else {
        return Ok(None);
    };
    let manifest = &writer.session.manifest;
    for target in query_targets(&manifest.workspace, options, request)? {
        if target.skip_reason.is_some() {
            continue;
        }
        let model_request = search_model_request(
            manifest,
            target.schema,
            options.embedding_concurrency,
            options,
            &location.root,
            uses_vectors(request),
        );
        if let Ok(model_request) = model_request
            && uses_vectors(request)
            && !writer
                .session
                .models
                .iter()
                .any(|model| model.matches_request(&model_request))
        {
            return Ok(None);
        }
    }
    let acquired = query_models(models, manifest, options, &location.root, request)?;
    query_storage(
        &acquired,
        &location.root,
        manifest,
        &writer.session.storage,
        options,
        request,
    )
    .await
    .map(Some)
}

fn uses_vectors(request: &super::context::NormalizedContextRequest) -> bool {
    request
        .routes
        .iter()
        .any(|route| route.mode == crate::api::context::options::ContextRouteMode::Vector)
}

struct QueryModel {
    info: EmbeddingModelInfo,
    model: Result<ModelRuntimeLease, EngineError>,
}

fn query_models(
    models: &ModelRuntimeManager,
    manifest: &WorkspaceManifest,
    options: &ContextOptions,
    root: &Path,
    request: &super::context::NormalizedContextRequest,
) -> Result<Vec<QueryModel>, EngineError> {
    let targets = query_targets(&manifest.workspace, options, request)?;
    if !uses_vectors(request) {
        return Ok(Vec::new());
    }
    let mut acquired: Vec<QueryModel> = Vec::new();
    for target in targets {
        if target.skip_reason.is_some()
            || acquired
                .iter()
                .any(|model| model.info.model.reference() == target.schema.model.reference())
        {
            continue;
        }
        let model = acquire_search_model_for(
            models,
            manifest,
            target.schema,
            options.embedding_concurrency,
            options,
            root,
        );
        if let Ok(model) = &model {
            assert_embedding_compatible(Some(manifest), model)?;
        }
        acquired.push(QueryModel {
            info: target.schema.clone(),
            model,
        });
    }
    Ok(acquired)
}

async fn query_storage(
    acquired: &[QueryModel],
    root: &Path,
    manifest: &WorkspaceManifest,
    storage: &IndexStore,
    options: &ContextOptions,
    request: &super::context::NormalizedContextRequest,
) -> Result<ContextResult, EngineError> {
    let descriptor = manifest
        .workspace
        .index
        .descriptor()
        .ok_or_else(|| EngineError::unsupported("workspace indexing is disabled"))?;
    storage.ensure_compatible(&descriptor.tables()?)?;
    let runtimes = acquired
        .iter()
        .map(|model| RequestEmbeddingRuntime {
            model: model.model.as_ref(),
            info: &model.info,
            signal: options.signal.clone(),
        })
        .collect::<Vec<_>>();
    let embedding_models = runtimes
        .iter()
        .map(|runtime| runtime as &dyn SearchEmbeddingRuntime)
        .collect::<Vec<_>>();
    context_from_index(
        root,
        &manifest.workspace,
        &manifest.path,
        manifest.storage_generation.as_deref().ok_or_else(|| {
            EngineError::storage_failure("indexed workspace has no storage generation")
        })?,
        storage,
        &embedding_models,
        options,
        request,
    )
    .await
}

pub(in crate::pipelines) fn refresh_options(
    options: &ContextOptions,
    root: PathBuf,
) -> IndexOptions {
    IndexOptions {
        root: Some(root),
        // Refresh reconciles the active index against the whole source tree.
        changes: vec![crate::api::index::options::WorkspaceChange::Rescan],
        on_progress: options.on_progress.clone(),
        signal: options.signal.clone(),
        allow_remote: options.allow_remote,
        authorized_remote: options.authorized_remote.clone(),
        // Refresh uses each model's persisted runtime configuration. Query-only
        // endpoint, credentials and device overrides must not reconfigure other routes.
        embedding_concurrency: options.embedding_concurrency,
        lock_timeout_ms: options.lock_timeout_ms,
        ..IndexOptions::default()
    }
}

pub(crate) struct QueryTarget<'a> {
    pub kind: ContentKind,
    pub schema: &'a EmbeddingModelInfo,
    pub skip_reason: Option<String>,
}

pub(crate) fn query_targets<'a>(
    workspace: &'a Workspace,
    options: &ContextOptions,
    request: &super::context::NormalizedContextRequest,
) -> Result<Vec<QueryTarget<'a>>, EngineError> {
    let descriptor = workspace
        .index
        .descriptor()
        .ok_or_else(|| EngineError::unsupported("workspace indexing is disabled"))?;
    let kinds = options.target_kind.map_or_else(
        || vec![ContentKind::Text, ContentKind::Code, ContentKind::Image],
        |kind| vec![kind],
    );
    let mut targets = Vec::new();
    for kind in kinds {
        let Some(schema) = descriptor.model_for(kind)? else {
            if options.target_kind.is_some() {
                return Err(EngineError::unsupported(format!(
                    "target {} is not enabled; configure its model route and rebuild",
                    kind.as_str()
                )));
            }
            continue;
        };
        let skip_reason = if kind == ContentKind::Image && !uses_vectors(request) {
            Some("image tables do not support full-text search".to_owned())
        } else if uses_vectors(request) && !schema.supports_retrieval(options.input_kind(), kind) {
            Some(format!(
                "{} does not support {} to {} retrieval",
                schema.model.reference(),
                options.input_kind().as_str(),
                kind.as_str()
            ))
        } else if request.image.as_ref().is_some_and(|image| {
            schema
                .max_image_bytes
                .is_some_and(|limit| image.data().len() > limit)
        }) {
            Some(format!(
                "query image exceeds the {} input size limit",
                schema.model.reference()
            ))
        } else {
            None
        };
        if options.target_kind.is_some()
            && let Some(reason) = &skip_reason
        {
            return Err(EngineError::unsupported(reason.clone()));
        }
        targets.push(QueryTarget {
            kind,
            schema,
            skip_reason,
        });
    }
    Ok(targets)
}

fn acquire_search_model_for(
    models: &ModelRuntimeManager,
    manifest: &WorkspaceManifest,
    schema: &crate::domain::EmbeddingModelInfo,
    embedding_concurrency: Option<usize>,
    options: &ContextOptions,
    root: &Path,
) -> Result<ModelRuntimeLease, EngineError> {
    models
        .acquire(search_model_request(
            manifest,
            schema,
            embedding_concurrency,
            options,
            root,
            true,
        )?)
        .map_err(ModelError::into_engine_error)
}

fn search_model_request(
    manifest: &WorkspaceManifest,
    schema: &crate::domain::EmbeddingModelInfo,
    embedding_concurrency: Option<usize>,
    options: &ContextOptions,
    root: &Path,
    authorize: bool,
) -> Result<ModelRuntimeRequest, EngineError> {
    let reference = schema.model.reference();
    let runtime = manifest
        .embedding_runtimes
        .get(&reference)
        .cloned()
        .unwrap_or_default();
    let config = crate::config::read()?;
    let local = schema.model.provider() == "local";
    if !local && options.device.is_some() {
        return Err(EngineError::invalid_argument(
            "--device is only supported for local embedding models",
        ));
    }
    let endpoint = if local {
        None
    } else {
        let endpoint = crate::authorization::remote_endpoint(
            &reference,
            options.endpoint.as_deref().or(runtime.endpoint.as_deref()),
        )?;
        if authorize {
            crate::authorization::require_with_targets(
                root,
                &reference,
                &endpoint,
                options.allow_remote,
                &options.authorized_remote,
            )?;
        }
        Some(endpoint)
    };
    Ok(ModelRuntimeRequest::new(
        reference.clone(),
        ModelConfig {
            api_key: (!local)
                .then(|| {
                    options
                        .api_key
                        .clone()
                        .or_else(|| runtime.api_key.clone())
                        .or_else(|| {
                            crate::config::string(
                                &config,
                                &["providers", schema.model.provider(), "apiKey"],
                            )
                        })
                        .or_else(environment_api_key)
                })
                .flatten(),
            endpoint,
            device: if local {
                crate::config::runtime_device(&config, &reference, options.device, runtime.device)?
            } else {
                None
            },
            cache_dir: crate::config::model_cache(
                &config,
                options.model_cache.clone(),
                runtime.cache_dir.clone(),
            ),
        },
        embedding_concurrency,
    ))
}

#[track_caller]
fn workspace_index_unavailable(root: &Path, reason: &str) -> EngineError {
    EngineError::not_found(format!("workspace index at {}: {reason}", root.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        IndexDescriptor, ScanRules,
        model::{EmbeddingMetric, ModelInfo},
    };

    fn model(name: &str, kinds: &[ContentKind]) -> EmbeddingModelInfo {
        EmbeddingModelInfo {
            space: crate::domain::model::EmbeddingSpace::fixture(),
            retrieval: crate::domain::model::EmbeddingRetrieval::TextImage,
            model: ModelInfo::new("test", name, kinds.iter().copied()).expect("model"),
            dimension: 4,
            metric: EmbeddingMetric::Cosine,
            max_batch_size: 1,
            max_input_tokens: None,
            max_image_bytes: Some(1024),
        }
    }

    #[test]
    fn targets_follow_explicit_selection_or_all_supported_tables() {
        let text = model("text", &[ContentKind::Text, ContentKind::Code]);
        let image = model("image", &[ContentKind::Text, ContentKind::Image]);
        let mut index = IndexDescriptor::single(text);
        let mut workspace = Workspace {
            name: "fixture".into(),
            root: PathBuf::from("/workspace"),
            scan: ScanRules::default(),
            index: IndexState::Enabled(Box::new(index.clone())),
            created_epoch_ms: 0,
            updated_epoch_ms: 0,
        };
        let mut options = ContextOptions {
            query: Some("orchard".into()),
            target_kind: Some(ContentKind::Image),
            ..ContextOptions::default()
        };
        let request = normalize_context_request(&options).expect("text query");
        let Err(error) = query_targets(&workspace, &options, &request) else {
            panic!("image target should not be enabled");
        };
        assert_eq!(error.code(), EngineError::UNSUPPORTED);
        assert!(error.message().contains("image"));

        index.routes.insert(ContentKind::Image, image);
        workspace.index = IndexState::Enabled(Box::new(index.clone()));
        let targets = query_targets(&workspace, &options, &request).expect("explicit image target");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].kind, ContentKind::Image);
        assert_eq!(targets[0].schema.model.reference(), "test/image");
        assert!(targets[0].skip_reason.is_none());
        assert_eq!(options.input_kind(), ContentKind::Text);

        options.target_kind = None;
        let targets = query_targets(&workspace, &options, &request).expect("default targets");
        assert_eq!(
            targets
                .iter()
                .map(|target| (target.kind, target.schema.model.reference()))
                .collect::<Vec<_>>(),
            vec![
                (ContentKind::Text, "test/text".into()),
                (ContentKind::Code, "test/text".into()),
                (ContentKind::Image, "test/image".into()),
            ],
        );
        assert!(targets.iter().all(|target| target.skip_reason.is_none()));

        index
            .routes
            .insert(ContentKind::Image, index.default_model.clone());
        workspace.index = IndexState::Enabled(Box::new(index));
        let Err(error) = query_targets(&workspace, &options, &request) else {
            panic!("explicit image route must support images");
        };
        assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
    }
}
