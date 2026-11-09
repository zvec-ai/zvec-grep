use super::spec::ArtifactSourceKind;
use crate::models::error::ModelError;

// Hyper-util exposes the CONNECT error through Service::Error, but does not
// re-export its name. Use that public associated type for structural matching.
type ProxyTunnelError = <hyper_util::client::legacy::connect::proxy::Tunnel<
    hyper_util::client::legacy::connect::HttpConnector,
> as tower_service::Service<http::Uri>>::Error;

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
    retry_ipv4: bool,
}

impl ArtifactDownloadError {
    pub(super) fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            source: None,
            artifact: None,
            status: None,
            message: message.into(),
            retry_ipv4: kind == FailureKind::Timeout,
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

    pub(super) fn ipv4_retry_allowed(&self) -> bool {
        self.retry_ipv4
    }

    pub(super) fn after_ipv4_retry(mut self, first: &Self) -> Self {
        self.message = format!(
            "unable to download model artifact; default connection failed: {}; IPv4 retry failed: {}",
            first.message, self.message
        );
        self.retry_ipv4 = false;
        self
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

/// Classify the underlying transport failure without parsing display strings or
/// exposing URLs, proxy credentials, or redirect query parameters in diagnostics.
pub(super) fn network_error(operation: &str, error: reqwest::Error) -> ArtifactDownloadError {
    let detail = if error.is_timeout() {
        Some("network timeout".to_owned())
    } else {
        transient_network_detail(&error)
    };
    let retry_ipv4 = detail.is_some();
    let mut message = format!("{operation}: {}", error.without_url());
    if let Some(detail) = detail {
        message.push_str(": ");
        message.push_str(&detail);
    }
    let mut failure = ArtifactDownloadError::new(FailureKind::Network, message);
    failure.retry_ipv4 = retry_ipv4;
    failure
}

fn transient_network_detail(error: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut current = Some(error);
    while let Some(cause) = current {
        if let Some(error) = cause.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind;
            if matches!(
                error.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::ConnectionRefused
                    | ErrorKind::BrokenPipe
                    | ErrorKind::NotConnected
                    | ErrorKind::TimedOut
                    | ErrorKind::UnexpectedEof
                    | ErrorKind::AddrNotAvailable
                    | ErrorKind::NetworkUnreachable
                    | ErrorKind::HostUnreachable
            ) {
                return Some(format!("transport {:?}", error.kind()));
            }
            // io::Error::source() can skip the wrapped error itself. Inspect
            // that value too so a nested transport error keeps its category.
            if let Some(inner) = error.get_ref() {
                current = Some(inner);
                continue;
            }
        }
        if let Some(error) = cause.downcast_ref::<hyper::Error>()
            && (error.is_incomplete_message() || error.is_closed())
        {
            return Some("connection closed before response completed".to_owned());
        }
        if matches!(
            cause.downcast_ref::<ProxyTunnelError>(),
            Some(ProxyTunnelError::TunnelUnexpectedEof)
        ) {
            return Some("proxy closed before CONNECT completed".to_owned());
        }
        current = cause.source();
    }
    None
}

#[cfg(test)]
mod tests;

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
