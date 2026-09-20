use std::panic::Location;

use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostErrorSite {
    file: &'static str,
    line: u32,
    column: u32,
}

impl HostErrorSite {
    #[track_caller]
    fn capture() -> Self {
        let location = Location::caller();
        Self {
            file: location.file(),
            line: location.line(),
            column: location.column(),
        }
    }

    #[must_use]
    pub const fn file(self) -> &'static str {
        self.file
    }

    #[must_use]
    pub const fn line(self) -> u32 {
        self.line
    }

    #[must_use]
    pub const fn column(self) -> u32 {
        self.column
    }
}

/// Failure produced by native filesystem scanning or watching.
#[derive(Debug, Error)]
pub enum HostError {
    #[error("invalid argument: {message}")]
    InvalidArgument {
        message: String,
        origin: HostErrorSite,
    },

    #[error("{component} failed: {message}")]
    StorageFailure {
        component: String,
        message: String,
        origin: HostErrorSite,
    },

    #[error("{message}")]
    Cancelled {
        message: String,
        origin: HostErrorSite,
    },

    #[error("{message}")]
    DeadlineExceeded {
        message: String,
        origin: HostErrorSite,
    },

    #[error("{message}")]
    ResourceClosed {
        message: String,
        origin: HostErrorSite,
    },

    #[error("internal error: {message}")]
    Internal {
        message: String,
        origin: HostErrorSite,
    },
}

impl HostError {
    #[track_caller]
    #[must_use]
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument {
            message: message.into(),
            origin: HostErrorSite::capture(),
        }
    }

    #[track_caller]
    #[must_use]
    pub fn storage_failure(component: impl Into<String>, message: impl Into<String>) -> Self {
        Self::StorageFailure {
            component: component.into(),
            message: message.into(),
            origin: HostErrorSite::capture(),
        }
    }

    #[track_caller]
    #[must_use]
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::Cancelled {
            message: message.into(),
            origin: HostErrorSite::capture(),
        }
    }

    #[track_caller]
    #[must_use]
    pub fn deadline_exceeded(message: impl Into<String>) -> Self {
        Self::DeadlineExceeded {
            message: message.into(),
            origin: HostErrorSite::capture(),
        }
    }

    #[track_caller]
    #[must_use]
    pub fn resource_closed(message: impl Into<String>) -> Self {
        Self::ResourceClosed {
            message: message.into(),
            origin: HostErrorSite::capture(),
        }
    }

    #[track_caller]
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
            origin: HostErrorSite::capture(),
        }
    }

    #[must_use]
    pub const fn origin(&self) -> HostErrorSite {
        match self {
            Self::InvalidArgument { origin, .. }
            | Self::StorageFailure { origin, .. }
            | Self::Cancelled { origin, .. }
            | Self::DeadlineExceeded { origin, .. }
            | Self::ResourceClosed { origin, .. }
            | Self::Internal { origin, .. } => *origin,
        }
    }
}
