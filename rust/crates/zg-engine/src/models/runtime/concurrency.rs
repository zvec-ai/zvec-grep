//! Index-only overrides and backend-specific runtime admission.

use std::env;

use super::llama_config::CONTEXT_PARALLELISM_ENV;
use crate::models::{EmbeddingCatalogEntry, ModelError, get_embedding_model_catalog_entry};

pub(crate) const EMBEDDING_CONCURRENCY_ENV: &str = "ZVEC_GREP_EMBEDDING_CONCURRENCY";
pub(crate) const LOCAL_CONCURRENCY_CAP: usize = 8;

pub(crate) fn is_llama(reference: &str) -> bool {
    matches!(
        get_embedding_model_catalog_entry(reference),
        Some(EmbeddingCatalogEntry::LlamaCpp(_))
    )
}

pub(super) fn is_transformers(reference: &str) -> bool {
    matches!(
        get_embedding_model_catalog_entry(reference),
        Some(EmbeddingCatalogEntry::Transformers(_))
    )
}

fn is_bekko(reference: &str) -> bool {
    matches!(
        get_embedding_model_catalog_entry(reference),
        Some(EmbeddingCatalogEntry::Bekko(_))
    )
}

/// Called only at the index boundary, including implicit builds and refreshes.
/// Omit the reference to select just the operation-wide CLI/environment limit.
pub(crate) fn resolve_index_concurrency(
    reference: Option<&str>,
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
    reference: Option<&str>,
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
    let mut name = EMBEDDING_CONCURRENCY_ENV;
    let mut value = environment(name).unwrap_or_default();
    if value.trim().is_empty() && reference.is_some_and(is_llama) {
        name = CONTEXT_PARALLELISM_ENV;
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

/// Budget normalization before backend-specific limits are known. Transformers
/// resolve their default and hardware limit on the model, including its provider.
pub(crate) fn local_runtime_limit(reference: &str, requested: Option<usize>) -> Option<usize> {
    if is_transformers(reference) {
        requested.map(|value| value.max(1))
    } else if is_bekko(reference) {
        Some(requested.unwrap_or(1).clamp(1, LOCAL_CONCURRENCY_CAP))
    } else {
        None
    }
}

/// Index admission is independent of the native execution budget. A llama.cpp
/// batch can use multiple contexts, so each operation submits one batch at a time.
pub(crate) fn batch_concurrency(reference: &str, requested: Option<usize>) -> Option<usize> {
    if is_llama(reference) {
        Some(1)
    } else if is_transformers(reference) {
        requested.map(|value| value.max(1))
    } else if is_bekko(reference) {
        Some(requested.unwrap_or(1).clamp(1, LOCAL_CONCURRENCY_CAP))
    } else {
        requested
    }
}

#[cfg(test)]
mod tests;
