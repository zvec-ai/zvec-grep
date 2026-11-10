//! Process-wide ORT compute configuration; model pools only own Sessions.
use std::{num::NonZeroUsize, sync::OnceLock};

use super::concurrency::EMBEDDING_CONCURRENCY_ENV;

pub(crate) const THREAD_BUDGET_ENV: &str = "ZVEC_GREP_ORT_THREAD_BUDGET";

fn hardware_threads() -> usize {
    std::thread::available_parallelism().map_or(1, NonZeroUsize::get)
}

fn positive(value: Option<&str>, default: usize) -> Option<usize> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Some(default),
        Some(value) => value.parse::<NonZeroUsize>().ok().map(NonZeroUsize::get),
    }
}

fn configured(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(value) => positive(Some(&value), default).unwrap_or_else(|| {
            tracing::warn!(
                environment = name,
                default,
                "Expected a positive integer; using default"
            );
            default
        }),
        Err(std::env::VarError::NotPresent) => default,
        Err(std::env::VarError::NotUnicode(_)) => {
            tracing::warn!(
                environment = name,
                default,
                "Invalid environment value; using default"
            );
            default
        }
    }
}

pub(crate) fn default_sessions() -> usize {
    session_count(configured(EMBEDDING_CONCURRENCY_ENV, 1))
}

pub(crate) fn session_count(requested: usize) -> usize {
    requested.clamp(1, hardware_threads())
}

/// Captured before serving requests, without loading the native library. The
/// one ORT Environment retains this pool across model eviction and reloading.
pub(crate) fn thread_budget() -> usize {
    static THREADS: OnceLock<usize> = OnceLock::new();
    *THREADS.get_or_init(|| {
        let requested = configured(THREAD_BUDGET_ENV, hardware_threads().div_ceil(2));
        let effective = requested.min(hardware_threads());
        if effective != requested {
            tracing::warn!(
                requested,
                effective,
                "ORT thread budget capped to available hardware threads"
            );
        }
        effective
    })
}

#[cfg(test)]
mod tests {
    use super::positive;

    #[test]
    fn defaults_and_positive_values_are_distinct_from_invalid_values() {
        assert_eq!(positive(None, 8), Some(8));
        assert_eq!(positive(Some(" "), 8), Some(8));
        assert_eq!(positive(Some(" 4 "), 8), Some(4));
        for value in ["0", "-1", "many", "184467440737095516160"] {
            assert_eq!(positive(Some(value), 8), None);
        }
    }
}
