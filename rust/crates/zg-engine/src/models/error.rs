use std::time::Duration;

use thiserror::Error;

use crate::{EngineError, ErrorSite};

#[derive(Debug, Error)]
#[error("{message}")]
pub struct ModelError {
    code: &'static str,
    message: String,
    context: Option<String>,
    cause: Option<String>,
    disposition: FailureDisposition,
    origin: Box<ErrorSite>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureDisposition {
    Input,
    Shared,
    Operation,
    Transient(Option<u32>),
    RateLimited(Option<u32>),
}

impl ModelError {
    #[track_caller]
    pub(crate) fn new(
        code: &'static str,
        message: impl Into<String>,
        context: Option<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            context,
            cause: None,
            disposition: FailureDisposition::Input,
            origin: Box::new(ErrorSite::capture()),
        }
    }

    #[track_caller]
    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(EngineError::INVALID_ARGUMENT, message, None)
    }

    #[track_caller]
    pub(crate) fn unsupported(message: impl Into<String>) -> Self {
        Self::new(EngineError::UNSUPPORTED, message, None)
    }

    #[track_caller]
    pub(crate) fn storage_failure(message: impl Into<String>) -> Self {
        Self::new(EngineError::STORAGE_FAILURE, message, None).shared()
    }

    #[track_caller]
    pub(crate) fn cancelled(message: impl Into<String>) -> Self {
        let mut error = Self::new(EngineError::CANCELLED, message, None);
        error.disposition = FailureDisposition::Operation;
        error
    }

    #[track_caller]
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(EngineError::INTERNAL, message, None)
    }

    pub(crate) fn with_cause(mut self, cause: impl std::fmt::Display) -> Self {
        self.cause = Some(cause.to_string());
        self
    }

    pub(crate) fn shared(mut self) -> Self {
        if self.disposition == FailureDisposition::Input {
            self.disposition = FailureDisposition::Shared;
        }
        self
    }

    pub(crate) fn transient(mut self, retry_after: Option<Duration>) -> Self {
        self.disposition = FailureDisposition::Transient(retry_after.map(retry_after_millis));
        self
    }

    pub(crate) fn rate_limited(mut self, retry_after: Option<Duration>) -> Self {
        self.disposition = FailureDisposition::RateLimited(retry_after.map(retry_after_millis));
        self
    }

    pub(crate) fn wrap(self, message: impl Into<String>, context: Option<String>) -> Self {
        let Self {
            code,
            message: cause_message,
            context: cause_context,
            cause,
            disposition,
            origin,
        } = self;
        Self {
            code,
            message: message.into(),
            context,
            cause: Some(compose_message(cause_message, cause_context, cause)),
            disposition,
            origin,
        }
    }

    pub(crate) fn into_engine_error(self) -> EngineError {
        let Self {
            code,
            message,
            context,
            cause,
            disposition: _,
            origin,
        } = self;
        EngineError::new_at(code, compose_message(message, context, cause), *origin)
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub fn context(&self) -> Option<&str> {
        self.context.as_deref()
    }

    #[must_use]
    pub fn cause(&self) -> Option<&str> {
        self.cause.as_deref()
    }

    #[must_use]
    pub(crate) const fn is_retryable(&self) -> bool {
        matches!(
            self.disposition,
            FailureDisposition::Transient(_) | FailureDisposition::RateLimited(_)
        )
    }

    #[must_use]
    pub(crate) const fn is_rate_limited(&self) -> bool {
        matches!(self.disposition, FailureDisposition::RateLimited(_))
    }

    #[must_use]
    pub(crate) fn retry_after(&self) -> Option<Duration> {
        let millis = match self.disposition {
            FailureDisposition::Transient(millis) | FailureDisposition::RateLimited(millis) => {
                millis
            }
            FailureDisposition::Input
            | FailureDisposition::Shared
            | FailureDisposition::Operation => None,
        };
        millis.map(|millis| Duration::from_millis(u64::from(millis)))
    }

    #[must_use]
    pub(crate) const fn should_fail_fast(&self) -> bool {
        !matches!(self.disposition, FailureDisposition::Input)
    }
}

fn retry_after_millis(duration: Duration) -> u32 {
    duration.as_millis().try_into().unwrap_or(u32::MAX)
}

fn compose_message(mut message: String, context: Option<String>, cause: Option<String>) -> String {
    if let Some(context) = context {
        message.push_str(": ");
        message.push_str(&context.replace('\n', "; "));
    }
    if let Some(cause) = cause {
        message.push_str("; cause: ");
        message.push_str(&cause);
    }
    message
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::ModelError;

    #[test]
    fn preserves_the_original_site_across_wrapping_and_conversion() {
        let origin_line = line!() + 1;
        let cause = ModelError::internal("model operation failed");
        let error = cause
            .wrap("embedding failed", Some("model=test".to_owned()))
            .into_engine_error();

        assert!(error.origin().file.ends_with("src/models/error.rs"));
        assert_eq!(error.origin().line, origin_line);
        assert_eq!(
            error.message(),
            "embedding failed: model=test; cause: model operation failed"
        );
    }

    #[test]
    fn wrapping_preserves_structured_failure_metadata() {
        let error = ModelError::internal("request timed out")
            .transient(Some(Duration::from_millis(25)))
            .wrap("embedding failed", None);

        assert!(error.is_retryable());
        assert!(!error.is_rate_limited());
        assert!(error.should_fail_fast());
        assert_eq!(error.retry_after(), Some(Duration::from_millis(25)));

        let input = ModelError::invalid_argument("bad input");
        assert!(!input.is_retryable());
        assert!(!input.should_fail_fast());

        let shared = ModelError::storage_failure("model load failed");
        assert!(shared.should_fail_fast());
    }
}
