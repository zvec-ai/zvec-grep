use thiserror::Error;

use crate::{EngineError, ErrorSite};

#[derive(Debug, Error)]
#[error("{message}")]
pub struct ModelError {
    code: &'static str,
    message: String,
    context: Option<String>,
    cause: Option<String>,
    origin: ErrorSite,
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
            origin: ErrorSite::capture(),
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
        Self::new(EngineError::STORAGE_FAILURE, message, None)
    }

    #[track_caller]
    pub(crate) fn cancelled(message: impl Into<String>) -> Self {
        Self::new(EngineError::CANCELLED, message, None)
    }

    #[track_caller]
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(EngineError::INTERNAL, message, None)
    }

    pub(crate) fn with_cause(mut self, cause: impl std::fmt::Display) -> Self {
        self.cause = Some(cause.to_string());
        self
    }

    pub(crate) fn wrap(self, message: impl Into<String>, context: Option<String>) -> Self {
        let Self {
            code,
            message: cause_message,
            context: cause_context,
            cause,
            origin,
        } = self;
        Self {
            code,
            message: message.into(),
            context,
            cause: Some(compose_message(cause_message, cause_context, cause)),
            origin,
        }
    }

    pub(crate) fn into_engine_error(self) -> EngineError {
        let Self {
            code,
            message,
            context,
            cause,
            origin,
        } = self;
        EngineError::new_at(code, compose_message(message, context, cause), origin)
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
}
