//! Fixed llama.cpp context capacity and CPU threads, owned by the runtime.
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::models::ModelError;

pub(crate) const THREADS_ENV: &str = "ZVEC_GREP_LLAMA_THREADS";
pub(crate) const SERVER_CONTEXTS_ENV: &str = "ZVEC_GREP_LLAMA_SERVER_CONTEXTS";

#[derive(Clone, Copy)]
pub(crate) struct LlamaSettings {
    pub(crate) server_contexts: usize,
    pub(crate) threads: Option<i32>,
    pub(crate) hardware_threads: usize,
}

fn positive(value: &str) -> Option<usize> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<usize>().ok().filter(|value| *value > 0)
}

fn configured(name: &str) -> Option<usize> {
    match std::env::var(name) {
        Ok(value) => positive(&value).or_else(|| {
            tracing::warn!(
                environment = name,
                "Expected a positive integer; using default"
            );
            None
        }),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            tracing::warn!(
                environment = name,
                "Invalid environment value; using default"
            );
            None
        }
    }
}

impl LlamaSettings {
    pub(crate) fn capture() -> Self {
        let threads = configured(THREADS_ENV).and_then(|value| {
            i32::try_from(value).ok().or_else(|| {
                tracing::warn!(
                    environment = THREADS_ENV,
                    "Thread count exceeds native limit; using default"
                );
                None
            })
        });
        Self {
            server_contexts: configured(SERVER_CONTEXTS_ENV).unwrap_or(1),
            threads,
            hardware_threads: std::thread::available_parallelism().map_or(1, usize::from),
        }
    }

    pub(crate) fn resources(self, contexts: usize) -> Arc<LlamaResources> {
        let contexts = contexts.max(1);
        let automatic = (self.hardware_threads / 2 / contexts).max(1);
        Arc::new(LlamaResources {
            contexts,
            cpu_threads: self
                .threads
                .unwrap_or_else(|| i32::try_from(automatic).unwrap_or(i32::MAX)),
            in_flight: Mutex::new(0),
            changed: Condvar::new(),
        })
    }
}

/// Server runtimes share this gate across models/devices. Direct runtimes own a
/// gate sized by that command's concurrency. This bounds executing contexts,
/// not resident weights/KV or OS threads, and is not a native thread pool.
pub(crate) struct LlamaResources {
    pub(crate) contexts: usize,
    pub(crate) cpu_threads: i32,
    in_flight: Mutex<usize>,
    changed: Condvar,
}

impl LlamaResources {
    pub(crate) fn acquire(
        &self,
        signal: Option<&CancellationToken>,
    ) -> Result<LlamaSlot<'_>, ModelError> {
        let mut count = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if signal.is_some_and(CancellationToken::is_cancelled) {
                return Err(ModelError::cancelled(
                    "llama.cpp context admission cancelled",
                ));
            }
            if *count < self.contexts {
                *count += 1;
                return Ok(LlamaSlot(self));
            }
            count = self
                .changed
                .wait_timeout(count, Duration::from_millis(10))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

pub(crate) struct LlamaSlot<'a>(&'a LlamaResources);

impl Drop for LlamaSlot<'_> {
    fn drop(&mut self) {
        let mut count = self
            .0
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *count -= 1;
        self.0.changed.notify_one();
    }
}

#[cfg(test)]
mod tests;
