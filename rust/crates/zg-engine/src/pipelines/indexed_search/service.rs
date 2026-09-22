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
    let schema = manifest.embedding().ok_or_else(|| {
        workspace_index_unavailable(&location.root, "writer model information is missing")
    })?;
    let model_request = search_model_request(
        manifest,
        schema,
        options.embedding_concurrency,
        options,
        &location.root,
        uses_vectors(request),
    )?;
    if !writer
        .session
        .models
        .first()
        .is_some_and(|model| model.matches_request(&model_request))
    {
        return Ok(None);
    }
    // Use the exact configuration that passed the writer-key check, including
    // authorization, rather than resolving mutable configuration a second time.
    let acquired = if uses_vectors(request) {
        let model = models
            .acquire(model_request)
            .map_err(ModelError::into_engine_error)?;
        assert_embedding_compatible(Some(manifest), &model)?;
        vec![model]
    } else {
        Vec::new()
    };
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

fn query_models(
    models: &ModelRuntimeManager,
    manifest: &WorkspaceManifest,
    options: &ContextOptions,
    root: &Path,
    request: &super::context::NormalizedContextRequest,
) -> Result<Vec<ModelRuntimeLease>, EngineError> {
    if !uses_vectors(request) {
        return Ok(Vec::new());
    }
    let model = acquire_search_model(
        models,
        manifest,
        options.embedding_concurrency,
        options,
        root,
    )?;
    assert_embedding_compatible(Some(manifest), &model)?;
    Ok(vec![model])
}

async fn query_storage(
    acquired: &[ModelRuntimeLease],
    root: &Path,
    manifest: &WorkspaceManifest,
    storage: &IndexStore,
    options: &ContextOptions,
    request: &super::context::NormalizedContextRequest,
) -> Result<ContextResult, EngineError> {
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
    context_from_index(
        root,
        &manifest.workspace,
        &manifest.path,
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
        api_key: options.api_key.clone(),
        endpoint: options.endpoint.clone(),
        embedding_concurrency: options.embedding_concurrency,
        lock_timeout_ms: options.lock_timeout_ms,
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
                                &["providers", &schema.model.provider, "apiKey"],
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
