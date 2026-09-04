//! Engine-wide error contract.

use std::{fmt, io, panic::Location};

use serde::{Deserialize, Serialize};

pub type EngineResult<T> = Result<T, EngineError>;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ErrorSite {
    pub file: String,
    pub line: u32,
    pub column: u32,
}

impl ErrorSite {
    #[track_caller]
    pub(crate) fn capture() -> Self {
        let location = Location::caller();
        Self::new(location.file(), location.line(), location.column())
    }

    #[must_use]
    pub fn new(file: &str, line: u32, column: u32) -> Self {
        Self {
            file: normalize_source_file(file),
            line,
            column,
        }
    }
}

impl fmt::Display for ErrorSite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}:{}", self.file, self.line, self.column)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ErrorReport {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    pub origin: ErrorSite,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<ErrorSite>,
}

impl ErrorReport {
    #[track_caller]
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self::new_at(code, message, ErrorSite::capture())
    }

    fn new_at(code: &'static str, message: impl Into<String>, origin: ErrorSite) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            help: None,
            origin,
            reported_at: None,
        }
    }
}

impl fmt::Display for ErrorReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "error[{}]: {}", self.code, self.message)?;
        if let Some(help) = &self.help {
            write!(formatter, "\nhelp: {help}")?;
        }
        write!(formatter, "\norigin: {}", self.origin)?;
        if let Some(reported_at) = &self.reported_at {
            write!(formatter, "\nreported at: {reported_at}")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct EngineError {
    report: Box<ErrorReport>,
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.report.message)
    }
}

impl std::error::Error for EngineError {}

impl EngineError {
    // Stable error codes, ordered from caller-facing to internal failures.

    pub const INVALID_ARGUMENT: &str = "ZG.ENGINE.INVALID_ARGUMENT";
    pub const NOT_FOUND: &str = "ZG.ENGINE.NOT_FOUND";
    pub const UNSUPPORTED: &str = "ZG.ENGINE.UNSUPPORTED";
    pub const PERMISSION_DENIED: &str = "ZG.ENGINE.PERMISSION_DENIED";
    pub const RESOURCE_BUSY: &str = "ZG.ENGINE.RESOURCE_BUSY";
    pub const RESOURCE_CLOSED: &str = "ZG.ENGINE.RESOURCE_CLOSED";
    pub const STORAGE_FAILURE: &str = "ZG.ENGINE.STORAGE_FAILURE";
    pub const CANCELLED: &str = "ZG.ENGINE.CANCELLED";
    pub const DEADLINE_EXCEEDED: &str = "ZG.ENGINE.DEADLINE_EXCEEDED";
    pub const INTERNAL: &str = "ZG.ENGINE.INTERNAL";

    // Public constructors, in the same order as the error codes above.

    #[track_caller]
    #[must_use]
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(Self::INVALID_ARGUMENT, message)
    }

    #[track_caller]
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Self::NOT_FOUND, message)
    }

    #[track_caller]
    #[must_use]
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(Self::UNSUPPORTED, message)
    }

    #[track_caller]
    #[must_use]
    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(Self::PERMISSION_DENIED, message)
    }

    #[track_caller]
    #[must_use]
    pub fn resource_busy(message: impl Into<String>) -> Self {
        Self::new(Self::RESOURCE_BUSY, message)
    }

    #[track_caller]
    #[must_use]
    pub fn resource_closed(message: impl Into<String>) -> Self {
        Self::new(Self::RESOURCE_CLOSED, message)
    }

    #[track_caller]
    #[must_use]
    pub fn storage_failure(message: impl Into<String>) -> Self {
        Self::new(Self::STORAGE_FAILURE, message)
    }

    #[track_caller]
    #[must_use]
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(Self::CANCELLED, message)
    }

    #[track_caller]
    #[must_use]
    pub fn deadline_exceeded(message: impl Into<String>) -> Self {
        Self::new(Self::DEADLINE_EXCEEDED, message)
    }

    #[track_caller]
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(Self::INTERNAL, message)
    }

    // Accessors.

    #[must_use]
    pub fn code(&self) -> &str {
        &self.report.code
    }

    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self.code(), Self::RESOURCE_BUSY | Self::DEADLINE_EXCEEDED)
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.report.message
    }

    #[must_use]
    pub fn help(&self) -> Option<&str> {
        self.report.help.as_deref()
    }

    #[must_use]
    pub const fn origin(&self) -> &ErrorSite {
        &self.report.origin
    }

    #[must_use]
    pub const fn reported_at(&self) -> Option<&ErrorSite> {
        self.report.reported_at.as_ref()
    }

    #[must_use]
    pub fn report(&self) -> ErrorReport {
        (*self.report).clone()
    }

    #[must_use]
    pub fn into_report(self) -> ErrorReport {
        *self.report
    }

    // Diagnostic enrichment.

    #[must_use]
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.report.help = Some(help.into());
        self
    }

    #[must_use]
    pub fn with_origin(mut self, origin: ErrorSite) -> Self {
        self.report.origin = origin;
        self
    }

    #[track_caller]
    #[must_use]
    pub(crate) fn report_here(mut self) -> Self {
        self.report.reported_at = Some(ErrorSite::capture());
        self
    }

    // Internal constructors and adapters.

    #[track_caller]
    #[must_use]
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            report: Box::new(ErrorReport::new(code, message)),
        }
    }

    #[must_use]
    pub(crate) fn new_at(
        code: &'static str,
        message: impl Into<String>,
        origin: ErrorSite,
    ) -> Self {
        Self {
            report: Box::new(ErrorReport::new_at(code, message, origin)),
        }
    }

    #[track_caller]
    #[must_use]
    pub(crate) fn from_io(context: impl Into<String>, error: &io::Error) -> Self {
        let message = format!("{}: {error}", context.into());
        match error.kind() {
            io::ErrorKind::NotFound => Self::not_found(message),
            io::ErrorKind::PermissionDenied => Self::permission_denied(message),
            io::ErrorKind::WouldBlock => Self::resource_busy(message),
            io::ErrorKind::TimedOut => Self::deadline_exceeded(message),
            _ => Self::storage_failure(message),
        }
    }
}

fn normalize_source_file(file: &str) -> String {
    let file = file.replace('\\', "/");
    file.find("crates/")
        .map_or_else(|| file.clone(), |index| file[index..].to_owned())
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::EngineError;

    #[test]
    fn exposes_readable_and_locatable_diagnostics() {
        let origin_line = line!() + 1;
        let error = EngineError::storage_failure("cannot open workspace manifest")
            .with_help("Check that the manifest exists and is readable.")
            .report_here();

        assert_eq!(error.code(), EngineError::STORAGE_FAILURE);
        assert_eq!(error.message(), "cannot open workspace manifest");
        assert_eq!(
            error.help(),
            Some("Check that the manifest exists and is readable.")
        );
        assert!(error.origin().file.ends_with("src/error.rs"));
        assert_eq!(error.origin().line, origin_line);
        assert!(error.reported_at().is_some());
        assert_eq!(error.to_string(), error.message());
        assert!(
            error
                .report()
                .to_string()
                .starts_with("error[ZG.ENGINE.STORAGE_FAILURE]:")
        );
    }

    #[test]
    fn serializes_the_transport_report() {
        let error = EngineError::not_found("workspace index does not exist")
            .with_help("Run `zg index` to build it.");
        let report = error.report();
        let value = serde_json::to_value(&report).expect("error report should serialize");

        assert_eq!(value["code"], "ZG.ENGINE.NOT_FOUND");
        assert_eq!(value["message"], "workspace index does not exist");
        assert_eq!(value["help"], "Run `zg index` to build it.");
        assert!(value.get("reported_at").is_none());

        let decoded = serde_json::from_value(value).expect("error report should deserialize");
        assert_eq!(report, decoded);
    }

    #[test]
    fn maps_actionable_io_errors_to_specific_codes() {
        let missing = EngineError::from_io(
            "failed to open workspace manifest",
            &io::Error::from(io::ErrorKind::NotFound),
        );
        let denied = EngineError::from_io(
            "failed to write workspace manifest",
            &io::Error::from(io::ErrorKind::PermissionDenied),
        );
        let busy = EngineError::from_io(
            "failed to acquire workspace lock",
            &io::Error::from(io::ErrorKind::WouldBlock),
        );
        let existing = EngineError::from_io(
            "failed to create workspace manifest",
            &io::Error::from(io::ErrorKind::AlreadyExists),
        );
        let timed_out = EngineError::from_io(
            "timed out reading workspace manifest",
            &io::Error::from(io::ErrorKind::TimedOut),
        );

        assert_eq!(missing.code(), EngineError::NOT_FOUND);
        assert_eq!(denied.code(), EngineError::PERMISSION_DENIED);
        assert_eq!(busy.code(), EngineError::RESOURCE_BUSY);
        assert_eq!(existing.code(), EngineError::STORAGE_FAILURE);
        assert_eq!(timed_out.code(), EngineError::DEADLINE_EXCEEDED);
        assert!(busy.is_retryable());
        assert!(timed_out.is_retryable());
        assert!(!existing.is_retryable());
    }
}
