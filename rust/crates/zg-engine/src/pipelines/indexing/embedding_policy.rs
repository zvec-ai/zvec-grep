//! Workspace-sensitive defaults without loading models or changing index state.

use std::{collections::HashMap, path::Path};

use tokio_util::sync::CancellationToken;
use zg_host_native::{NativeScanner, ScanRequest, TaskControl, WorkspaceScannerPort};

use crate::{
    EngineError,
    api::index::{
        IndexOptions,
        embedding::{
            EmbeddingAnalysis, EmbeddingEvidence, EmbeddingScene, EmbeddingSelection,
            EmbeddingSelectionSource,
        },
    },
    config,
    domain::{FileCategory, FileFormat, IndexDescriptor, ScanRules, Workspace},
    extraction::{SourceKind, source_kind},
    file_selection::ScanPolicy,
    models::{ModelError, ResolveEmbeddingReferenceOptions, resolve_embedding_reference},
};

use super::pipeline::{default_file_size_limit, map_host_error, throw_if_cancelled};

pub(super) const CODE_MODEL: &str = "local/potion-code-16m-v2";
const TEXT_MODEL: &str = "local/bekko-256-int8";
const FILE_WEIGHT_LIMIT: u64 = 64 * 1024;

/// Remote-consent checks only need configured destinations: automatic choices
/// are always local, so preflight never needs to scan the workspace.
pub(crate) fn configured_selection(
    existing: Option<&Workspace>,
    options: &IndexOptions,
) -> Result<Option<EmbeddingSelection>, EngineError> {
    let config = config::read()?;
    let existing = existing
        .and_then(|workspace| workspace.index().descriptor())
        .map(IndexDescriptor::default_embedding_model)
        .map(|model| model.model().reference());
    if options.embedding.is_none()
        && existing.is_none()
        && let Some(default) = &options.embedding_default
    {
        if matches!(
            default.source,
            EmbeddingSelectionSource::Explicit | EmbeddingSelectionSource::ExistingIndex
        ) {
            return Err(EngineError::invalid_argument(
                "Caller embedding defaults cannot be explicit or existing-index selections",
            ));
        }
        return Ok(Some(default.clone()));
    }
    choose_configured(
        options
            .embedding
            .as_ref()
            .map(|model| model.reference.clone()),
        existing,
        std::env::var("ZVEC_GREP_EMBEDDING").unwrap_or_default(),
        config::string(&config, &["defaults", "embedding"]),
    )
}

fn choose_configured(
    explicit: Option<String>,
    existing: Option<String>,
    environment: String,
    global_default: Option<String>,
) -> Result<Option<EmbeddingSelection>, EngineError> {
    let source = if explicit.is_some() {
        EmbeddingSelectionSource::Explicit
    } else if existing.is_some() {
        EmbeddingSelectionSource::ExistingIndex
    } else if !environment.trim().is_empty() {
        EmbeddingSelectionSource::Environment
    } else {
        EmbeddingSelectionSource::Configuration
    };
    let reference = resolve_embedding_reference(ResolveEmbeddingReferenceOptions {
        explicit,
        existing,
        environment: Some(HashMap::from([("ZVEC_GREP_EMBEDDING".into(), environment)])),
        global_default,
        fallback: None,
    })
    .map_err(ModelError::into_engine_error)?;
    Ok(reference.map(|model| EmbeddingSelection {
        model,
        source,
        analysis: None,
    }))
}

pub(super) async fn select(
    root: &Path,
    scan: &ScanRules,
    existing: Option<&Workspace>,
    options: &IndexOptions,
) -> Result<EmbeddingSelection, EngineError> {
    let configured = configured_selection(existing, options)?;
    if configured.as_ref().is_some_and(|selection| {
        options.embedding_default.is_some()
            || matches!(
                selection.source,
                EmbeddingSelectionSource::Explicit | EmbeddingSelectionSource::ExistingIndex
            )
    }) {
        return Ok(configured.expect("resolved selection"));
    }
    let analysis = analyze(root, scan, options.signal.clone()).await?;
    let mut selection = configured.unwrap_or_else(|| EmbeddingSelection {
        model: analysis.recommended_model.clone(),
        source: EmbeddingSelectionSource::Automatic,
        analysis: None,
    });
    selection.analysis = Some(analysis);
    Ok(selection)
}

async fn analyze(
    root: &Path,
    scan: &ScanRules,
    signal: Option<CancellationToken>,
) -> Result<EmbeddingAnalysis, EngineError> {
    throw_if_cancelled(signal.as_ref())?;
    let control = TaskControl::new(signal.clone().unwrap_or_default());
    let snapshot = NativeScanner::default()
        .discover(
            &ScanRequest {
                roots: vec![ScanPolicy::root_spec(root, scan)?],
                scope_paths: Vec::new(),
            },
            &control,
        )
        .await
        .map_err(map_host_error)?;
    let scan = scan.clone();
    tokio::task::spawn_blocking(move || {
        let mut code = EmbeddingEvidence::default();
        let mut text = EmbeddingEvidence::default();
        let mut other_files = 0;
        for file in snapshot.files {
            throw_if_cancelled(signal.as_ref())?;
            let Ok(formats) = FileFormat::from_path(&file.root.join(&file.relative_path)) else {
                // The indexing pass still owns per-file error reporting and retries.
                other_files += 1;
                continue;
            };
            if !matches!(source_kind(&formats), Some(SourceKind::Text))
                || file.size_bytes
                    > scan
                        .max_file_size_bytes()
                        .unwrap_or_else(|| default_file_size_limit(&formats))
                || file.size_bytes == 0
            {
                continue;
            }
            let has = |category| {
                formats
                    .iter()
                    .any(|format| format.categories().contains(&category))
            };
            let evidence = match (
                has(FileCategory::Code),
                has(FileCategory::Document),
                has(FileCategory::Data),
            ) {
                (true, false, false) => &mut code,
                (false, true, false) => &mut text,
                _ => {
                    other_files += 1;
                    continue;
                }
            };
            evidence.files += 1;
            evidence.weight_bytes = evidence
                .weight_bytes
                .saturating_add(file.size_bytes.min(FILE_WEIGHT_LIMIT));
        }
        throw_if_cancelled(signal.as_ref())?;
        Ok(summarize(code, text, other_files))
    })
    .await
    .map_err(|error| EngineError::internal(format!("embedding analysis worker failed: {error}")))?
}

fn summarize(
    code: EmbeddingEvidence,
    text: EmbeddingEvidence,
    other_files: usize,
) -> EmbeddingAnalysis {
    let code_weight = u128::from(code.weight_bytes);
    let text_weight = u128::from(text.weight_bytes);
    let total = code_weight + text_weight;
    let scene = if total == 0 {
        EmbeddingScene::Unknown
    } else if code_weight * 100 >= total * 60 {
        EmbeddingScene::Code
    } else if text_weight * 100 >= total * 60 {
        EmbeddingScene::Text
    } else {
        EmbeddingScene::Mixed
    };
    EmbeddingAnalysis {
        scene,
        recommended_model: if text_weight > code_weight {
            TEXT_MODEL
        } else {
            CODE_MODEL
        }
        .into(),
        code,
        text,
        other_files,
    }
}

#[cfg(test)]
mod tests;
