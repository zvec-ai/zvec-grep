//! Artifact source locations and safe cache path resolution.

use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use super::{
    error::{ArtifactDownloadError, FailureKind},
    spec::{ArtifactConfig, ArtifactSourceConfig, ArtifactSourceKind},
};

const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub(crate) struct ArtifactSource {
    pub(super) kind: ArtifactSourceKind,
    pub(super) repo: &'static str,
    pub(super) revision: &'static str,
    pub(super) cache_directory: PathBuf,
    pub(super) local_paths: BTreeMap<&'static str, PathBuf>,
    pub(super) base_url: Option<String>,
    pub(super) response_header_timeout: Duration,
    pub(super) read_idle_timeout: Duration,
}

impl ArtifactSource {
    pub(crate) fn hugging_face(config: ArtifactSourceConfig, cache_directory: PathBuf) -> Self {
        Self::new(ArtifactSourceKind::HuggingFace, config, cache_directory)
    }

    pub(crate) fn model_scope(config: ArtifactSourceConfig, cache_directory: PathBuf) -> Self {
        Self::new(ArtifactSourceKind::ModelScope, config, cache_directory)
    }

    fn new(
        kind: ArtifactSourceKind,
        config: ArtifactSourceConfig,
        cache_directory: PathBuf,
    ) -> Self {
        Self {
            kind,
            repo: config.repo,
            revision: config.revision,
            cache_directory,
            local_paths: BTreeMap::new(),
            base_url: None,
            response_header_timeout: RESPONSE_HEADER_TIMEOUT,
            read_idle_timeout: READ_IDLE_TIMEOUT,
        }
    }

    pub(crate) fn with_local_path(
        mut self,
        artifact: &'static str,
        local_path: impl Into<PathBuf>,
    ) -> Self {
        self.local_paths.insert(artifact, local_path.into());
        self
    }

    #[cfg(test)]
    pub(super) fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    #[cfg(test)]
    pub(super) fn with_timeouts(mut self, response_header: Duration, read_idle: Duration) -> Self {
        self.response_header_timeout = response_header;
        self.read_idle_timeout = read_idle;
        self
    }

    pub(super) fn local_path(
        &self,
        artifact: &ArtifactConfig,
    ) -> Result<PathBuf, ArtifactDownloadError> {
        safe_local_path(
            &self.cache_directory,
            self.local_paths
                .get(artifact.path)
                .map_or_else(|| Path::new(artifact.path), PathBuf::as_path),
        )
    }
}

fn safe_local_path(base: &Path, relative: &Path) -> Result<PathBuf, ArtifactDownloadError> {
    validate_relative_path(relative)?;
    Ok(base.join(relative))
}

pub(super) fn validate_relative_path(path: &Path) -> Result<(), ArtifactDownloadError> {
    let normalized = path.to_str().map(|value| value.replace('\\', "/"));
    if normalized.as_deref().is_none_or(|value| {
        value.is_empty()
            || value.contains('\0')
            || value.starts_with('/')
            || value
                .split('/')
                .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    }) || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ArtifactDownloadError::new(
            FailureKind::InvalidInput,
            format!("invalid relative artifact path '{}'", path.display()),
        ));
    }
    Ok(())
}
