//! Resolve the indexed workspace, refresh it when requested, and execute retrieval.

use std::path::{Path, PathBuf};

use crate::{
    EngineError,
    api::{
        context::{ContextOptions, ContextResult},
        index::IndexOptions,
    },
    domain::{ContentKind, EmbeddingModelInfo, IndexState, Workspace, model::ModelConfig},
    models::{
        EmbeddingCatalogEntry, ModelError, ModelRuntimeLease, ModelRuntimeManager,
        ModelRuntimeRequest, get_embedding_model_catalog_entry,
    },
    pipelines::indexing::service::{
        WorkspaceIndexService, assert_embedding_compatible, environment_api_key, is_indexed,
        workspace_api_key,
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
    let Some(location) = find_nearest_workspace(&requested_root, options.index_dir.as_deref())?
    else {
        return Err(workspace_index_unavailable(
            &requested_root,
            "no workspace manifest was found",
        ));
    };
    let initial_manifest = initial_query_manifest(&location)?;
    validate_query_source(&location, initial_manifest.as_ref())?;
    let _binding = crate::workspace::lock::bind_source(&location.home, &location.root)?;
    if let Some(manifest) = initial_manifest
        .as_ref()
        .filter(|manifest| is_indexed(manifest))
    {
        query_targets(&manifest.workspace, options, &request)?;
    }
    if !refresh_completed
        && options.refresh.map_or(options.auto_update, |policy| {
            policy == crate::api::context::options::RefreshPolicy::Wait
        })
        && indexing.workspace_needs_refresh(&location, &wait).await?
    {
        indexing
            .index(
                models,
                refresh_options(options, location.root.clone(), location.home.clone()),
            )
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
    let manifest = read_workspace_manifest(&location)?.ok_or_else(|| {
        workspace_index_unavailable(&location.root, "workspace manifest disappeared")
    })?;
    validate_query_source(&location, Some(&manifest))?;
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
    if !IndexStore::exists(&manifest.index_path())? {
        return Err(workspace_index_unavailable(
            &location.root,
            "index storage is missing",
        ));
    }
    let acquired = query_models(models, &manifest, options, &location.root, &request)?;
    let storage = match read_sessions {
        Some(cache) => cache.acquire(&location.home, &manifest.index_path())?,
        None => ReadSessionLease::open(&manifest.index_path())?,
    };
    storage.verify_source_root(&location.root)?;
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

fn validate_query_source(
    location: &crate::workspace::layout::WorkspaceIndexLocation,
    manifest: Option<&WorkspaceManifest>,
) -> Result<(), EngineError> {
    if manifest.is_some_and(|manifest| manifest.workspace.root != location.root) {
        return Err(EngineError::resource_busy(
            "workspace source directory changed; retry the query",
        ));
    }
    crate::workspace::layout::validate_source_root(location)
}

fn initial_query_manifest(
    location: &crate::workspace::layout::WorkspaceIndexLocation,
) -> Result<Option<WorkspaceManifest>, EngineError> {
    let initial_manifest = read_workspace_manifest(location)?;
    if initial_manifest.is_none() && !crate::workspace::build::has_build(&location.home) {
        if crate::workspace::build::has_index_storage(&location.home)? {
            return Err(EngineError::storage_failure(
                "index storage has no usable manifest; rebuild the index with `zg --index --rebuild`",
            ));
        }
        return Err(workspace_index_unavailable(
            &location.root,
            "no workspace manifest was found in the selected index directory",
        ));
    }
    Ok(initial_manifest)
}

async fn try_writer_context(
    indexing: &WorkspaceIndexService,
    models: &ModelRuntimeManager,
    location: &crate::workspace::layout::WorkspaceIndexLocation,
    options: &ContextOptions,
    request: &super::context::NormalizedContextRequest,
) -> Result<Option<ContextResult>, EngineError> {
    let Some(active) = read_workspace_manifest(location)? else {
        return Ok(None);
    };
    if !is_indexed(&active) {
        return Ok(None);
    }
    let Some(writer) = indexing
        .writers
        .borrow(&location.home, active.storage_generation.as_deref())
    else {
        return Ok(None);
    };
    let manifest = &writer.session.manifest;
    if manifest.workspace.root != location.root {
        return Err(EngineError::resource_busy(
            "active writer uses a different source root",
        ));
    }
    let requests = query_model_requests(manifest, options, &location.root, request)?;
    for requested in &requests {
        if let Ok(request) = &requested.request
            && !writer
                .session
                .models
                .iter()
                .any(|model| model.matches_query_request(request))
        {
            return Ok(None);
        }
    }
    let acquired = acquire_query_models(models, manifest, requests)?;
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
    acquire_query_models(
        models,
        manifest,
        query_model_requests(manifest, options, root, request)?,
    )
}

struct QueryModelRequest<'a> {
    info: &'a EmbeddingModelInfo,
    request: Result<ModelRuntimeRequest, EngineError>,
}

fn query_model_requests<'a>(
    manifest: &'a WorkspaceManifest,
    options: &ContextOptions,
    root: &Path,
    request: &super::context::NormalizedContextRequest,
) -> Result<Vec<QueryModelRequest<'a>>, EngineError> {
    let targets = query_targets(&manifest.workspace, options, request)?;
    if !uses_vectors(request) {
        return Ok(Vec::new());
    }
    let mut requests: Vec<QueryModelRequest<'_>> = Vec::new();
    for target in targets {
        if target.skip_reason.is_some()
            || requests
                .iter()
                .any(|model| model.info.model().reference() == target.schema.model().reference())
        {
            continue;
        }
        requests.push(QueryModelRequest {
            info: target.schema,
            request: search_model_request(manifest, target.schema, options, root, true),
        });
    }
    Ok(requests)
}

fn acquire_query_models(
    models: &ModelRuntimeManager,
    manifest: &WorkspaceManifest,
    requests: Vec<QueryModelRequest<'_>>,
) -> Result<Vec<QueryModel>, EngineError> {
    requests
        .into_iter()
        .map(|requested| {
            let model = requested.request.and_then(|request| {
                models
                    .acquire(request)
                    .map_err(ModelError::into_engine_error)
            });
            if let Ok(model) = &model {
                assert_embedding_compatible(Some(manifest), model)?;
            }
            Ok(QueryModel {
                info: requested.info.clone(),
                model,
            })
        })
        .collect()
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
    storage.ensure_compatible(&descriptor.content_indexes())?;
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
    index_dir: PathBuf,
) -> IndexOptions {
    IndexOptions {
        root: Some(root),
        index_dir: Some(index_dir),
        runtime_device: options.device,
        model_cache: options.model_cache.clone(),
        // Refresh reconciles the active index against the whole source tree.
        changes: vec![crate::api::index::options::WorkspaceChange::Rescan],
        on_progress: options.on_progress.clone(),
        signal: options.signal.clone(),
        allow_remote: options.allow_remote,
        authorized_remote: options.authorized_remote.clone(),
        // Query-only endpoints and credentials do not reconfigure saved routes.
        // Device and cache overrides remain local runtime settings.
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
        let Some(schema) = descriptor.model_for(kind) else {
            if options.target_kind.is_some() {
                return Err(EngineError::unsupported(format!(
                    "target {} is not enabled; configure its model route and rebuild",
                    kind.as_str()
                )));
            }
            continue;
        };
        let current = get_embedding_model_catalog_entry(&schema.model().reference())
            .map(EmbeddingCatalogEntry::embedding_model_info)
            .transpose()?;
        let info = current.as_ref().unwrap_or(schema);
        let skip_reason = if kind == ContentKind::Image && !uses_vectors(request) {
            Some("image tables do not support full-text search".to_owned())
        } else if uses_vectors(request) && !info.model().supports_content(kind) {
            Some(format!(
                "{} no longer supports the {} table's content",
                info.model().reference(),
                kind.as_str()
            ))
        } else if uses_vectors(request) && !info.model().supports_content(options.input_kind()) {
            Some(format!(
                "{} does not support {} query input for the {} table",
                schema.model().reference(),
                options.input_kind().as_str(),
                kind.as_str()
            ))
        } else if request.image.as_ref().is_some_and(|image| {
            info.max_image_bytes()
                .is_some_and(|limit| image.data().len() > limit)
        }) {
            Some(format!(
                "query image exceeds the {} input size limit",
                schema.model().reference()
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

fn search_model_request(
    manifest: &WorkspaceManifest,
    schema: &crate::domain::EmbeddingModelInfo,
    options: &ContextOptions,
    root: &Path,
    authorize: bool,
) -> Result<ModelRuntimeRequest, EngineError> {
    let reference = schema.model().reference();
    let runtime = manifest
        .embedding_runtimes
        .get(&reference)
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
        if authorize {
            crate::authorization::require_with_targets(
                root,
                Some(&manifest.path),
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
            api_key: if local {
                None
            } else {
                workspace_api_key(Some(manifest), &reference, options.api_key.as_deref())?
                    .or_else(|| {
                        crate::config::string(
                            &config,
                            &["providers", schema.model().provider(), "apiKey"],
                        )
                    })
                    .or_else(environment_api_key)
            },
            endpoint,
            device: if local {
                crate::config::runtime_device(&config, &reference, options.device)?
            } else {
                None
            },
            cache_dir: crate::config::model_cache(&config, options.model_cache.clone()),
        },
        None,
    ))
}

#[track_caller]
fn workspace_index_unavailable(root: &Path, reason: &str) -> EngineError {
    EngineError::not_found(format!("workspace index at {}: {reason}", root.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::domain::{
        IndexDescriptor, ScanRules,
        model::{EmbeddingMetric, ModelInfo},
    };

    fn model(name: &str, kinds: &[ContentKind]) -> EmbeddingModelInfo {
        EmbeddingModelInfo::new(
            ModelInfo::new("test", name, kinds.iter().copied(), 1).expect("model"),
            4,
            EmbeddingMetric::Cosine,
            None,
            Some(1024),
        )
        .expect("embedding model")
    }

    #[test]
    fn targets_follow_explicit_selection_or_all_supported_tables() {
        let text = model("text", &[ContentKind::Text, ContentKind::Code]);
        let image = model("image", &[ContentKind::Text, ContentKind::Image]);
        let mut index = IndexDescriptor::new(text, BTreeMap::new(), crate::domain::FTS_CONFIG)
            .expect("index descriptor");
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

        index = IndexDescriptor::new(
            index.default_embedding_model().clone(),
            BTreeMap::from([(ContentKind::Image, image)]),
            index.fts(),
        )
        .expect("image route");
        workspace.index = IndexState::Enabled(Box::new(index.clone()));
        let targets = query_targets(&workspace, &options, &request).expect("explicit image target");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].kind, ContentKind::Image);
        assert_eq!(targets[0].schema.model().reference(), "test/image");
        assert!(targets[0].skip_reason.is_none());
        assert_eq!(options.input_kind(), ContentKind::Text);

        options.target_kind = None;
        let targets = query_targets(&workspace, &options, &request).expect("default targets");
        assert_eq!(
            targets
                .iter()
                .map(|target| (target.kind, target.schema.model().reference()))
                .collect::<Vec<_>>(),
            vec![
                (ContentKind::Text, "test/text".into()),
                (ContentKind::Code, "test/text".into()),
                (ContentKind::Image, "test/image".into()),
            ],
        );
        assert!(targets.iter().all(|target| target.skip_reason.is_none()));

        let error = IndexDescriptor::new(
            index.default_embedding_model().clone(),
            BTreeMap::from([(ContentKind::Image, index.default_embedding_model().clone())]),
            index.fts(),
        )
        .expect_err("explicit image route must support images");
        assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
    }

    #[test]
    fn image_query_admission_uses_current_catalog_capabilities_and_limits() {
        use crate::api::context::options::QueryImage;

        let current = get_embedding_model_catalog_entry("local/embeddinggemma-2")
            .expect("local multimodal catalog entry")
            .embedding_model_info()
            .expect("current metadata");
        let persisted = EmbeddingModelInfo::new(
            ModelInfo::new(
                current.model().provider(),
                current.model().name(),
                [ContentKind::Text, ContentKind::Code],
                current.model().max_batch_size(),
            )
            .expect("previous model capabilities"),
            current.dimension(),
            current.metric(),
            current.max_input_tokens(),
            Some(1),
        )
        .expect("previous embedding model");
        let workspace = Workspace {
            name: "fixture".into(),
            root: PathBuf::from("/workspace"),
            scan: ScanRules::default(),
            index: IndexState::Enabled(Box::new(
                IndexDescriptor::new(persisted, BTreeMap::new(), crate::domain::FTS_CONFIG)
                    .expect("index descriptor"),
            )),
            created_epoch_ms: 0,
            updated_epoch_ms: 0,
        };
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::new(2, 2))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("small query image");
        let data = bytes.into_inner();
        assert!(data.len() > 1);
        assert!(data.len() < current.max_image_bytes().expect("current image limit"));
        let mut options = ContextOptions {
            query_image: Some(QueryImage::Bytes {
                format: crate::domain::FileFormat::Png,
                data,
            }),
            ..ContextOptions::default()
        };
        let request = normalize_context_request(&options).expect("image query");
        assert_eq!(options.input_kind(), ContentKind::Image);

        for target_kind in [Some(ContentKind::Text), None] {
            options.target_kind = target_kind;
            let targets = query_targets(&workspace, &options, &request)
                .expect("current model accepts the image query");
            let expected = if target_kind.is_some() {
                vec![ContentKind::Text]
            } else {
                vec![ContentKind::Text, ContentKind::Code]
            };
            assert_eq!(
                targets.iter().map(|target| target.kind).collect::<Vec<_>>(),
                expected
            );
            assert!(targets.iter().all(|target| target.skip_reason.is_none()));
        }
    }
}
