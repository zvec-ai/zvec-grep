//! Index-only overrides and backend-specific runtime admission.

use std::env;

use crate::models::{EmbeddingCatalogEntry, ModelError, get_embedding_model_catalog_entry};

pub(crate) const INDEX_CONCURRENCY_ENV: &str = "ZVEC_GREP_INDEX_EMBEDDING_CONCURRENCY";
const LEGACY_LLAMA_ENV: &str = "ZVEC_GREP_LLAMA_CONTEXT_PARALLELISM";
pub(crate) const LOCAL_CONCURRENCY_CAP: usize = 8;

pub(crate) fn is_llama(reference: &str) -> bool {
    matches!(
        get_embedding_model_catalog_entry(reference),
        Some(EmbeddingCatalogEntry::LlamaCpp(_))
    )
}

fn is_transformers(reference: &str) -> bool {
    matches!(
        get_embedding_model_catalog_entry(reference),
        Some(EmbeddingCatalogEntry::Transformers(_))
    )
}

/// Called only at the index boundary, including implicit builds and refreshes.
pub(crate) fn resolve_index_concurrency(
    reference: &str,
    requested: Option<usize>,
) -> Result<Option<usize>, ModelError> {
    resolve_override(
        reference,
        requested,
        |name| env::var(name).ok(),
        |message| {
            tracing::warn!("{message}");
        },
    )
}

fn resolve_override(
    reference: &str,
    requested: Option<usize>,
    mut environment: impl FnMut(&str) -> Option<String>,
    mut warn: impl FnMut(String),
) -> Result<Option<usize>, ModelError> {
    if requested == Some(0) {
        return Err(ModelError::invalid_argument(
            "Embedding concurrency must be greater than zero",
        ));
    }
    if requested.is_some() {
        return Ok(requested);
    }
    let mut name = INDEX_CONCURRENCY_ENV;
    let mut value = environment(name).unwrap_or_default();
    if value.trim().is_empty() && is_llama(reference) {
        name = LEGACY_LLAMA_ENV;
        value = environment(name).unwrap_or_default();
    }
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.bytes().all(|byte| byte.is_ascii_digit())
        && let Ok(parsed) = value.parse::<usize>()
        && parsed > 0
    {
        return Ok(Some(parsed));
    }
    warn(format!(
        "invalid {name}=\"{value}\", using automatic parallelism"
    ));
    Ok(None)
}

/// Local native resources must not be shared across different fixed budgets.
pub(crate) fn local_runtime_limit(reference: &str, requested: Option<usize>) -> Option<usize> {
    if is_transformers(reference) || is_llama(reference) {
        Some(requested.unwrap_or(1).clamp(1, LOCAL_CONCURRENCY_CAP))
    } else {
        None
    }
}

/// llama.cpp parallelizes contexts within a single batch; never multiply them
/// by allowing several batches to execute on the same cached model.
pub(crate) fn batch_concurrency(reference: &str, requested: Option<usize>) -> Option<usize> {
    if is_llama(reference) {
        Some(1)
    } else if is_transformers(reference) {
        Some(requested.unwrap_or(1).clamp(1, LOCAL_CONCURRENCY_CAP))
    } else {
        requested
    }
}

#[cfg(test)]
mod tests;
