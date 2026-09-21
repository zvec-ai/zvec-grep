//! Resolve the indexed workspace, refresh it when requested, and execute retrieval.

use std::path::{Path, PathBuf};

use crate::{
    EngineError,
    api::{
        context::{ContextOptions, ContextResult},
        index::IndexOptions,
    },
    domain::{IndexState, model::ModelConfig},
    models::{ModelError, ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest},
    pipelines::indexing::service::{
        WorkspaceIndexService, assert_embedding_compatible, environment_api_key, is_indexed,
    },
    storage::{IndexStore, types::WorkspaceIndexStorageOptions},
    workspace::{
        layout::find_nearest_workspace,
        lock::{LockMode, acquire_home_lock},
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
) -> Result<ContextResult, EngineError> {
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
    if options.refresh.map_or(options.auto_update, |policy| {
        policy == crate::api::context::options::RefreshPolicy::Wait
    }) && indexing.workspace_needs_refresh(&location).await?
    {
        indexing
            .index(models, refresh_options(options, location.root.clone()))
            .await?;
    }
    let _lock = acquire_home_lock(&location.home, LockMode::Read, "context")?;
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
    let mut acquired = Vec::new();
    if request
        .routes
        .iter()
        .any(|route| route.mode == crate::api::context::options::ContextRouteMode::Vector)
    {
        let model = acquire_search_model(
            models,
            &manifest,
            options.embedding_concurrency,
            options,
            &location.root,
        )?;
        assert_embedding_compatible(Some(&manifest), &model)?;
        acquired.push(model);
    }
    let runtimes = acquired
        .iter()
        .map(|model| RequestEmbeddingRuntime {
            model,
            signal: options.signal.clone(),
        })
        .collect::<Vec<_>>();
    let embedding_models = runtimes
        .iter()
        .map(|runtime| runtime as &dyn SearchEmbeddingRuntime)
        .collect::<Vec<_>>();
    let storage = IndexStore::open(WorkspaceIndexStorageOptions::ReadOnly {
        storage_path: manifest.storage_home(),
    })?;
    let result = context_from_index(
        &location.root,
        &manifest.workspace,
        &manifest.path,
        &storage,
        &embedding_models,
        options,
        &request,
    )
    .await;
    let close = storage.close();
    let result = result?;
    close?;
    Ok(result)
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
        api_key: options.api_key.clone(),
        endpoint: options.endpoint.clone(),
        embedding_concurrency: options.embedding_concurrency,
        device: options.device,
        model_cache: options.model_cache.clone(),
        ..IndexOptions::default()
    }
}

pub(in crate::pipelines) fn acquire_search_model(
    models: &ModelRuntimeManager,
    manifest: &WorkspaceManifest,
    embedding_concurrency: Option<usize>,
    options: &ContextOptions,
    root: &Path,
) -> Result<ModelRuntimeLease, EngineError> {
    let schema = manifest.embedding().ok_or_else(|| {
        workspace_index_unavailable(&manifest.path, "embedding model information is missing")
    })?;
    acquire_search_model_for(
        models,
        manifest,
        schema,
        embedding_concurrency,
        options,
        root,
    )
}

fn acquire_search_model_for(
    models: &ModelRuntimeManager,
    manifest: &WorkspaceManifest,
    schema: &crate::domain::EmbeddingModelInfo,
    embedding_concurrency: Option<usize>,
    options: &ContextOptions,
    root: &Path,
) -> Result<ModelRuntimeLease, EngineError> {
    let reference = schema.model.reference();
    if options
        .authorization_model
        .as_ref()
        .is_some_and(|expected| expected != &reference)
    {
        return Err(EngineError::permission_denied(
            "Workspace embedding model changed after authorization; retry the query",
        ));
    }
    let runtime = manifest
        .embedding_runtimes
        .get(&reference)
        .cloned()
        .unwrap_or_default();
    let config = crate::config::read()?;
    let local = schema.model.provider == "local";
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
        crate::authorization::require_with_targets(
            root,
            &reference,
            &endpoint,
            options.allow_remote,
            &options.authorized_remote,
        )?;
        Some(endpoint)
    };
    models
        .acquire(ModelRuntimeRequest::new(
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
                                    &["providers", &schema.model.provider, "apiKey"],
                                )
                            })
                            .or_else(environment_api_key)
                    })
                    .flatten(),
                endpoint,
                device: if local {
                    crate::config::runtime_device(
                        &config,
                        &reference,
                        options.device,
                        runtime.device,
                    )?
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
        .map_err(ModelError::into_engine_error)
}

#[track_caller]
fn workspace_index_unavailable(root: &Path, reason: &str) -> EngineError {
    EngineError::not_found(format!("workspace index at {}: {reason}", root.display()))
}
