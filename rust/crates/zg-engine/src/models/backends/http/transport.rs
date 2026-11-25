use std::time::{Duration, SystemTime};

use reqwest::{
    Client, StatusCode, Url,
    header::{self, HeaderValue},
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    EngineError,
    models::spi::{EmbeddingTraceHeaders, ModelError},
};

pub(super) struct HttpTransport {
    client: Client,
    endpoint: Url,
    authorization: Option<HeaderValue>,
}

impl HttpTransport {
    pub(super) fn new(
        endpoint: Url,
        authorization: Option<HeaderValue>,
        timeout: Duration,
    ) -> Result<Self, ModelError> {
        let client = Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            // The indexing scheduler owns the bounded retry budget.
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| {
                ModelError::internal("Unable to initialize HTTP embedding client").shared()
            })?;
        Ok(Self {
            client,
            endpoint,
            authorization,
        })
    }

    pub(super) async fn post(
        &self,
        body: &Value,
        signal: Option<&CancellationToken>,
        trace_headers: Option<&EmbeddingTraceHeaders>,
    ) -> Result<Vec<u8>, ModelError> {
        let future = async {
            let body = serde_json::to_vec(body).map_err(|_| {
                ModelError::internal("Unable to serialize HTTP embedding request").shared()
            })?;
            let mut request = self
                .client
                .post(self.endpoint.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .body(body);
            if let Some(authorization) = &self.authorization {
                request = request.header(header::AUTHORIZATION, authorization.clone());
            }
            if let Some(headers) = trace_headers {
                request = request.header("traceparent", &headers.traceparent);
                if let Some(value) = &headers.tracestate {
                    request = request.header("tracestate", value);
                }
                if let Some(value) = &headers.baggage {
                    request = request.header("baggage", value);
                }
            }
            let response = request
                .send()
                .await
                .map_err(|error| transport_error(&error))?;
            let status = response.status();
            if !status.is_success() {
                // Classify solely from status; error bodies can echo credentials and inputs.
                let retry_after = response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(retry_after);
                return Err(status_error(status, retry_after));
            }
            response
                .bytes()
                .await
                .map(|bytes| bytes.to_vec())
                .map_err(|error| transport_error(&error))
        };
        match signal {
            Some(signal) => tokio::select! {
                biased;
                () = signal.cancelled() => Err(ModelError::cancelled("HTTP embedding request was cancelled")),
                result = future => result,
            },
            None => future.await,
        }
    }
}

fn transport_error(error: &reqwest::Error) -> ModelError {
    let timeout = error.is_timeout();
    let transient = !error.is_builder()
        && (timeout || error.is_connect() || error.is_request() || error.is_body());
    let error = ModelError::new(
        if timeout {
            EngineError::DEADLINE_EXCEEDED
        } else {
            EngineError::INTERNAL
        },
        if timeout {
            "HTTP embedding request timed out"
        } else {
            "HTTP embedding transport failed"
        },
        None,
    );
    // reqwest errors may include URLs and nested messages; never retain their Display/Debug.
    if transient {
        error.transient(None)
    } else {
        error.shared()
    }
}

fn status_error(status: StatusCode, retry_after: Option<Duration>) -> ModelError {
    let code = match status.as_u16() {
        400 | 413 | 422 => EngineError::INVALID_ARGUMENT,
        401 | 403 => EngineError::PERMISSION_DENIED,
        404 => EngineError::NOT_FOUND,
        405 | 501 => EngineError::UNSUPPORTED,
        408 | 504 => EngineError::DEADLINE_EXCEEDED,
        409 | 423 | 429 => EngineError::RESOURCE_BUSY,
        _ => EngineError::INTERNAL,
    };
    let error = ModelError::new(
        code,
        "HTTP embedding request failed",
        Some(format!("status={}", status.as_u16())),
    );
    if status == StatusCode::TOO_MANY_REQUESTS {
        error.rate_limited(retry_after)
    } else if status == StatusCode::REQUEST_TIMEOUT || status.is_server_error() {
        error.transient(retry_after)
    } else if matches!(status.as_u16(), 413 | 422) {
        error
    } else {
        error.shared()
    }
}

fn retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<f64>()
        && seconds.is_finite()
        && seconds >= 0.0
    {
        // Match ModelError's millisecond ceiling without overflowing Duration on hostile headers.
        let millis = (seconds * 1000.0).min(f64::from(u32::MAX));
        let millis = format!("{millis:.0}").parse::<u32>().ok()?;
        return Some(Duration::from_millis(u64::from(millis)));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(date.duration_since(SystemTime::now()).unwrap_or_default())
}
