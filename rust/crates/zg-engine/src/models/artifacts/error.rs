use super::spec::ArtifactSourceKind;
use crate::models::error::ModelError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FailureKind {
    Http,
    Network,
    Timeout,
    Integrity,
    Filesystem,
    Cancelled,
    Callback,
    InvalidInput,
}

#[derive(Debug)]
pub(crate) struct ArtifactDownloadError {
    kind: FailureKind,
    source: Option<ArtifactSourceKind>,
    artifact: Option<String>,
    status: Option<u16>,
    message: String,
}

impl ArtifactDownloadError {
    pub(super) fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            source: None,
            artifact: None,
            status: None,
            message: message.into(),
        }
    }

    pub(super) fn at_source(mut self, source: ArtifactSourceKind) -> Self {
        self.source = Some(source);
        self
    }

    pub(super) fn at_artifact(mut self, artifact: impl Into<String>) -> Self {
        self.artifact = Some(artifact.into());
        self
    }

    pub(super) fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub(super) fn fallback_allowed(&self) -> bool {
        matches!(
            self.kind,
            FailureKind::Network | FailureKind::Timeout | FailureKind::Integrity
        ) || (self.kind == FailureKind::Http
            && self.status.is_some_and(|status| {
                matches!(status, 403 | 404 | 408 | 429) || (500..=599).contains(&status)
            }))
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.kind == FailureKind::Cancelled
    }

    pub(super) fn into_model_error(self) -> ModelError {
        if self.kind == FailureKind::Cancelled {
            ModelError::cancelled(self.to_string())
        } else {
            ModelError::storage_failure(self.to_string())
        }
    }
}

impl std::fmt::Display for ArtifactDownloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.message)?;
        if let Some(source) = self.source {
            write!(formatter, "; source={}", source.label())?;
        }
        if let Some(artifact) = &self.artifact {
            write!(formatter, "; artifact={artifact}")?;
        }
        if let Some(status) = self.status {
            write!(formatter, "; status={status}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ArtifactDownloadError {}

pub(super) fn filesystem_error(
    source: ArtifactSourceKind,
    artifact: Option<&str>,
    operation: &str,
    error: impl std::fmt::Display,
) -> ArtifactDownloadError {
    let error = ArtifactDownloadError::new(
        FailureKind::Filesystem,
        format!("unable to {operation}: {error}"),
    )
    .at_source(source);
    match artifact {
        Some(artifact) => error.at_artifact(artifact),
        None => error,
    }
}
