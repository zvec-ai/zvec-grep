//! Resolve the indexed workspace, refresh it when requested, and execute retrieval.

use std::path::{Path, PathBuf};

use crate::{
    EngineError,
    api::{
        context::{ContextOptions, ContextResult},
        index::IndexOptions,
    },
    domain::{IndexState, Workspace, WorkspaceLayout, model::ModelConfig},
    models::{ModelError, ModelRuntimeLease, ModelRuntimeManager, ModelRuntimeRequest},
    pipelines::indexing::service::{
        WorkspaceIndexService, assert_embedding_compatible, environment_api_key,
    },
    storage::{
        IndexStore,
        read_session::{ReadSessionCache, ReadSessionLease},
    },
    utils::{directory_exists, resolve_directory},
    workspace::{
        credentials::decrypt_api_key,
        find_nearest_workspace,
        lock::{LockWait, try_acquire_workspace_read},
        manifest::read_workspace_manifest,
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
    let mut options = std::borrow::Cow::Borrowed(options);
    if let Some(cache_dir) = &options.model_cache {
        let cache_dir = std::path::absolute(cache_dir).map_err(|error| {
            EngineError::from_io("failed to resolve model cache directory", &error)
        })?;
        options.to_mut().model_cache = Some(cache_dir);
    }
    let options = options.as_ref();
    let wait = LockWait::new(options.signal.as_ref(), options.lock_timeout_ms)?;
    wait.check_cancelled()?;
    let requested_root = resolve_directory(options.root.as_deref())?;
    let request = normalize_context_request(options)?;
    let Some((source_root, layout)) = find_nearest_workspace(&requested_root)? else {
        return Err(workspace_index_unavailable(
            &requested_root,
            "no workspace manifest was found",
        ));
    };
    if !refresh_completed
        && options.refresh.map_or(options.auto_update, |policy| {
            policy == crate::api::context::options::RefreshPolicy::Wait
        })
        && indexing
            .workspace_needs_refresh(&source_root, &layout, &wait, options.signal.clone())
            .await?
    {
        indexing
            .index(models, refresh_options(options, source_root.clone()))
            .await?;
    }
    let wait = LockWait::new(options.signal.as_ref(), options.lock_timeout_ms)?;
    let allows_writer = options.refresh.map_or(!options.auto_update, |policy| {
        policy != crate::api::context::options::RefreshPolicy::Wait
    });
    let _lock = loop {
        wait.check_cancelled()?;
        if allows_writer
            && let Some(result) =
                try_writer_context(indexing, models, &source_root, &layout, options, &request)
                    .await?
        {
            return Ok(result);
        }
        if let Some(lock) = try_acquire_workspace_read(&source_root, &layout, "context")? {
            break lock;
        }
        wait.wait_for_retry(layout.storage_dir(), "context").await?;
    };
    let workspace = read_workspace_manifest(&source_root, &layout)?.ok_or_else(|| {
        workspace_index_unavailable(&source_root, "workspace manifest disappeared")
    })?;
    if workspace.index() == &IndexState::Disabled {
        return Err(EngineError::unsupported(format!(
            "workspace indexing is disabled at {}",
            source_root.display()
        )));
    }
    if !workspace.index_enabled() {
        return Err(workspace_index_unavailable(
            &source_root,
            "workspace index has not been built",
        ));
    }
    if !directory_exists(workspace.layout().index_dir())? {
        return Err(workspace_index_unavailable(
            &source_root,
            "index storage is missing",
        ));
    }
    let acquired = query_models(models, &workspace, options, &request)?;
    let storage = match read_sessions {
        Some(cache) => cache.acquire(
            &layout,
            workspace.layout().index_dir(),
            workspace
                .storage_generation()
                .expect("published index generation"),
        )?,
        None => ReadSessionLease::open(workspace.layout().index_dir())?,
    };
    let result = query_storage(&acquired, &workspace, storage.storage(), options, &request).await;
    let close = storage.close();
    let result = result?;
    close?;
    Ok(result)
}

async fn try_writer_context(
    indexing: &WorkspaceIndexService,
    models: &ModelRuntimeManager,
    source_root: &Path,
    layout: &WorkspaceLayout,
    options: &ContextOptions,
    request: &super::context::NormalizedContextRequest,
) -> Result<Option<ContextResult>, EngineError> {
    let Some(writer) = indexing.writers.borrow(layout.storage_dir()) else {
        return Ok(None);
    };
    let Some(active) = read_workspace_manifest(source_root, layout)? else {
        return Ok(None);
    };
    let workspace = &writer.session.workspace;
    if !active.index_enabled() || active.storage_generation() != workspace.storage_generation() {
        return Ok(None);
    }
    let acquired = if uses_vectors(request) {
        let schema = workspace
            .index()
            .descriptor()
            .map(crate::domain::IndexDescriptor::default_embedding_model)
            .ok_or_else(|| {
                workspace_index_unavailable(source_root, "writer model information is missing")
            })?;
        let model_request = search_model_request(workspace, schema, options)?;
        if !writer
            .session
            .models
            .first()
            .is_some_and(|model| model.matches_query_request(&model_request))
        {
            return Ok(None);
        }
        // Reuse the configuration that passed authorization and the writer-key check.
        let model = models
            .acquire(model_request)
            .map_err(ModelError::into_engine_error)?;
        assert_embedding_compatible(workspace.index().descriptor(), model.info())?;
        vec![model]
    } else {
        Vec::new()
    };
    query_storage(
        &acquired,
        workspace,
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
    workspace: &Workspace,
    options: &ContextOptions,
    request: &super::context::NormalizedContextRequest,
) -> Result<Vec<ModelRuntimeLease>, EngineError> {
    if !uses_vectors(request) {
        return Ok(Vec::new());
    }
    let model = acquire_search_model(models, workspace, options)?;
    assert_embedding_compatible(workspace.index().descriptor(), model.info())?;
    Ok(vec![model])
}

async fn query_storage(
    acquired: &[ModelRuntimeLease],
    workspace: &Workspace,
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
        workspace.source_root(),
        workspace,
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
        require_existing_index: true,
        // Refresh reconciles the active index against the whole source tree.
        changes: vec![crate::api::index::options::WorkspaceChange::Rescan],
        on_progress: options.on_progress.clone(),
        signal: options.signal.clone(),
        allow_remote: options.allow_remote,
        authorized_remote: options.authorized_remote.clone(),
        embedding_concurrency: options.embedding_concurrency,
        lock_timeout_ms: options.lock_timeout_ms,
        runtime_device: options.device,
        runtime_model_cache: options.model_cache.clone(),
        ..IndexOptions::default()
    }
}

pub(in crate::pipelines) fn acquire_search_model(
    models: &ModelRuntimeManager,
    workspace: &Workspace,
    options: &ContextOptions,
) -> Result<ModelRuntimeLease, EngineError> {
    let schema = workspace
        .index()
        .descriptor()
        .map(crate::domain::IndexDescriptor::default_embedding_model)
        .ok_or_else(|| {
            workspace_index_unavailable(
                workspace.layout().storage_dir(),
                "embedding model information is missing",
            )
        })?;
    acquire_search_model_for(models, workspace, schema, options)
}

fn acquire_search_model_for(
    models: &ModelRuntimeManager,
    workspace: &Workspace,
    schema: &crate::domain::EmbeddingModelInfo,
    options: &ContextOptions,
) -> Result<ModelRuntimeLease, EngineError> {
    models
        .acquire(search_model_request(workspace, schema, options)?)
        .map_err(ModelError::into_engine_error)
}

fn search_model_request(
    workspace: &Workspace,
    schema: &crate::domain::EmbeddingModelInfo,
    options: &ContextOptions,
) -> Result<ModelRuntimeRequest, EngineError> {
    let reference = schema.model().reference();
    if options
        .authorization_model
        .as_ref()
        .is_some_and(|expected| expected != &reference)
    {
        return Err(EngineError::permission_denied(
            "Workspace embedding model changed after authorization; retry the query",
        ));
    }
    let runtime = workspace
        .index()
        .descriptor()
        .and_then(|index| index.embedding_configs().get(&reference))
        .cloned()
        .unwrap_or_default();
    let config = crate::config::read()?;
    let local = schema.model().provider() == "local";
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
            workspace.source_root(),
            &reference,
            &endpoint,
            options.allow_remote,
            &options.authorized_remote,
        )?;
        Some(endpoint)
    };
    let saved_key = if local || options.api_key.is_some() {
        None
    } else {
        runtime
            .api_key
            .as_ref()
            .map(|key| decrypt_api_key(key, workspace.layout(), &reference))
            .transpose()?
    };
    Ok(ModelRuntimeRequest::new(
        reference.clone(),
        ModelConfig {
            api_key: (!local)
                .then(|| {
                    options
                        .api_key
                        .clone()
                        .or(saved_key)
                        .or_else(|| {
                            crate::config::string(
                                &config,
                                &["providers", schema.model().provider(), "apiKey"],
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
                runtime.cache_dir,
            ),
        },
        None,
    ))
}

#[track_caller]
fn workspace_index_unavailable(root: &Path, reason: &str) -> EngineError {
    EngineError::not_found(format!("workspace index at {}: {reason}", root.display()))
}
