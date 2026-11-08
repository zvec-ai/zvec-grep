//! Model artifact download, validation, locking, and publication.

mod cache_lock;
mod cache_paths;
mod downloader;
mod error;
mod identity;
mod inspection;
mod manifest;
mod progress;
mod publish;
mod source;
mod spec;

pub(super) use cache_paths::{LocalModelBackend, default_model_cache};
pub(super) use downloader::{HTTP_USER_AGENT, ResolveArtifacts, resolve_model_artifacts};
#[cfg(test)]
pub(super) use inspection::cached_source;
pub(crate) use inspection::model_is_cached;
#[cfg(test)]
pub(super) use progress::ArtifactDownloadProgress;
pub(super) use progress::ModelDownloadProgressReporter;
pub(super) use source::ArtifactSource;
pub(super) use spec::{ArtifactConfig, ArtifactDownloadConfig, ArtifactSourceConfig};
