use crate::DaemonError;
use std::path::Path;

/// Resolves and validates an optional Bearer token without logging its value.
/// # Errors
/// Returns an error for unreadable files or invalid tokens.
pub fn resolve_token(file: Option<&Path>) -> Result<Option<String>, DaemonError> {
    let token = if let Ok(value) = std::env::var("ZVEC_GREP_SERVER_TOKEN") {
        Some(value)
    } else {
        let environment =
            std::env::var_os("ZVEC_GREP_SERVER_TOKEN_FILE").map(std::path::PathBuf::from);
        file.or(environment.as_deref())
            .map(std::fs::read_to_string)
            .transpose()?
            .map(|value| value.trim().to_owned())
    };
    if let Some(token) = &token
        && (token.len() < 32 || token.bytes().any(|byte| byte.is_ascii_control()))
    {
        return Err(DaemonError::McpBridge(
            "Server token must contain at least 32 characters and no control characters".into(),
        ));
    }
    Ok(token)
}

pub(crate) async fn authenticate(
    axum::extract::State(expected): axum::extract::State<Option<String>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if request.uri().path() != "/healthz"
        && let Some(expected) = expected
    {
        let supplied = request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        let valid = supplied.is_some_and(|value| {
            value.len() == expected.len()
                && value
                    .bytes()
                    .zip(expected.bytes())
                    .fold(0u8, |difference, (a, b)| difference | (a ^ b))
                    == 0
        });
        if !valid {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
                "unauthorized",
            )
                .into_response();
        }
    }
    next.run(request).await
}
