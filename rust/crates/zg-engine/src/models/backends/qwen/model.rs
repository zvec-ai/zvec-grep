use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::domain::{Content, FileFormat};

use crate::domain::model::{EmbeddingModelInfo, EmbeddingResult, ModelConfig, ModelInfo};
use crate::models::{
    catalog::QwenConfig,
    spi::{
        EmbeddingConcurrencyDefaults, EmbeddingModel, EmbeddingOptions, EmbeddingTraceHeaders,
        ModelError, input_text, validate_inputs, validate_result,
    },
};

const REMOTE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_MULTIMODAL_IMAGES: usize = 10;

pub(crate) struct QwenEmbeddingModel {
    entry: QwenConfig,
    info: EmbeddingModelInfo,
    api_key: String,
    endpoint: String,
    http: Arc<dyn QwenHttpClient>,
}

impl QwenEmbeddingModel {
    pub(crate) fn new(entry: QwenConfig, options: ModelConfig) -> Result<Self, ModelError> {
        Self::with_http(entry, options, Arc::new(ReqwestQwenHttpClient::new()?))
    }

    fn with_http(
        entry: QwenConfig,
        options: ModelConfig,
        http: Arc<dyn QwenHttpClient>,
    ) -> Result<Self, ModelError> {
        let display_name = model_name(entry);
        let api_key = options.api_key.unwrap_or_default().trim().to_owned();
        if api_key.is_empty() {
            return Err(ModelError::new(
                crate::EngineError::PERMISSION_DENIED,
                format!("{display_name} model requires an API key"),
                Some(format!(
                    "model={}\nhint=Pass --api-key, set ZVEC_GREP_API_KEY, or configure the qwen provider API key.",
                    entry.reference
                )),
            )
            .shared());
        }
        let endpoint = options.endpoint.map_or_else(
            || entry.default_endpoint.to_owned(),
            |value| value.trim().to_owned(),
        );
        if endpoint.is_empty() {
            return Err(ModelError::new(
                crate::EngineError::INVALID_ARGUMENT,
                format!("{display_name} model requires an endpoint"),
                Some(format!("model={}", entry.reference)),
            )
            .shared());
        }
        Ok(Self {
            entry,
            info: EmbeddingModelInfo {
                model: ModelInfo {
                    provider: entry.provider.to_owned(),
                    name: entry.model.to_owned(),
                    endpoint: Some(endpoint.clone()),
                },
                dimension: entry.dimension,
                metric: entry.metric,
                max_batch_size: entry.max_batch_size,
                max_input_tokens: Some(entry.max_input_tokens),
                max_image_bytes: entry.max_image_bytes,
            },
            api_key,
            endpoint,
            http,
        })
    }

    async fn embed_text(
        &self,
        inputs: &[Vec<Content>],
        signal: Option<CancellationToken>,
        trace_headers: Option<EmbeddingTraceHeaders>,
    ) -> Result<EmbeddingResult, ModelError> {
        let texts = inputs
            .iter()
            .map(|input| input_text(input))
            .collect::<Result<Vec<_>, _>>()?;
        let request = json!({
            "model": self.entry.model,
            "input": texts,
            "dimensions": self.info.dimension,
            "encoding_format": "float",
        });
        let response = self.send(request, signal, trace_headers).await?;
        let body = parse_response_body(&response, self.entry)?;
        if !response.success() {
            return Err(provider_error(self.entry, &response, &body));
        }
        let data = body.get("data").and_then(Value::as_array).ok_or_else(|| {
            ModelError::new(
                crate::EngineError::INTERNAL,
                format!("{} response did not include data", model_name(self.entry)),
                Some(format!("model={}", self.entry.reference)),
            )
        })?;
        let mut vectors = vec![None; inputs.len()];
        for item in data {
            let object = item
                .as_object()
                .ok_or_else(|| invalid_text_index(self.entry, "unknown"))?;
            let index = object
                .get("index")
                .and_then(json_integer)
                .ok_or_else(|| invalid_text_index(self.entry, "unknown"))?;
            let index = usize::try_from(index)
                .map_err(|_| index_out_of_range(self.entry, index, inputs.len()))?;
            if index >= inputs.len() {
                return Err(index_out_of_range(self.entry, index, inputs.len()));
            }
            let vector = parse_vector(object.get("embedding"), self.entry, index)?;
            vectors[index] = Some(vector);
        }
        Ok(EmbeddingResult {
            vectors: collect_vectors(vectors, self.entry)?,
            truncated: Vec::new(),
        })
    }

    async fn embed_multimodal(
        &self,
        inputs: &[Vec<Content>],
        signal: Option<CancellationToken>,
        trace_headers: Option<EmbeddingTraceHeaders>,
    ) -> Result<EmbeddingResult, ModelError> {
        validate_multimodal_inputs(self.entry, inputs)?;
        let request_contents = inputs
            .iter()
            .map(|input| match input.as_slice() {
                [Content::Text(text)] => Ok(json!({ "text": text })),
                [Content::Image(image)] => Ok(json!({ "image": bytes_to_base64(image.data()) })),
                _ => Err(ModelError::unsupported(
                    "Qwen multimodal embedding requires one text or image content per input",
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let request = json!({
            "model": self.entry.model,
            "input": { "contents": request_contents },
            "parameters": { "dimension": self.info.dimension },
        });
        let response = self.send(request, signal, trace_headers).await?;
        let body = parse_response_body(&response, self.entry)?;
        if !response.success() {
            return Err(provider_error(self.entry, &response, &body));
        }
        let items = body
            .get("output")
            .and_then(|output| output.get("embeddings"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                ModelError::new(
                    crate::EngineError::INTERNAL,
                    "Qwen3 VL embedding response did not include embeddings",
                    Some(format!("model={}", self.entry.reference)),
                )
            })?;
        let mut vectors = vec![None; inputs.len()];
        for (fallback_index, item) in items.iter().enumerate() {
            let object = item.as_object().ok_or_else(|| {
                ModelError::new(
                    crate::EngineError::INTERNAL,
                    "Qwen3 VL embedding response included an invalid embedding item",
                    Some(format!(
                        "model={} index={fallback_index}",
                        self.entry.reference
                    )),
                )
            })?;
            let raw_index = object
                .get("index")
                .and_then(json_integer)
                .or_else(|| object.get("text_index").and_then(json_integer))
                .unwrap_or_else(|| i64::try_from(fallback_index).unwrap_or(i64::MAX));
            let index = usize::try_from(raw_index)
                .map_err(|_| multimodal_index_out_of_range(self.entry, raw_index, inputs.len()))?;
            if index >= inputs.len() {
                return Err(multimodal_index_out_of_range(
                    self.entry,
                    index,
                    inputs.len(),
                ));
            }
            vectors[index] = Some(parse_vector(object.get("embedding"), self.entry, index)?);
        }
        Ok(EmbeddingResult {
            vectors: collect_vectors(vectors, self.entry)?,
            truncated: Vec::new(),
        })
    }

    async fn send(
        &self,
        body: Value,
        signal: Option<CancellationToken>,
        trace_headers: Option<EmbeddingTraceHeaders>,
    ) -> Result<QwenHttpResponse, ModelError> {
        if signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
            return Err(ModelError::cancelled("embedding request was cancelled"));
        }
        self.http
            .post(QwenHttpRequest {
                endpoint: self.endpoint.clone(),
                bearer_token: self.api_key.clone(),
                body,
                signal,
                trace_headers,
            })
            .await
    }
}

#[async_trait]
impl EmbeddingModel for QwenEmbeddingModel {
    fn info(&self) -> &EmbeddingModelInfo {
        &self.info
    }

    fn concurrency_defaults(&self) -> EmbeddingConcurrencyDefaults {
        if self.entry.kind == "multimodal" {
            EmbeddingConcurrencyDefaults {
                initial: 4,
                maximum: 8,
            }
        } else {
            EmbeddingConcurrencyDefaults {
                initial: 8,
                maximum: 12,
            }
        }
    }

    async fn embed(
        &self,
        inputs: &[Vec<Content>],
        options: EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError> {
        validate_inputs(&self.info, inputs, |content| match content {
            Content::Text(_) => true,
            Content::Image(_) => self.entry.kind == "multimodal",
            Content::Table(_) => false,
        })?;
        let EmbeddingOptions {
            signal,
            trace_headers,
            ..
        } = options;
        let result = if self.entry.kind == "multimodal" {
            self.embed_multimodal(inputs, signal, trace_headers).await?
        } else {
            self.embed_text(inputs, signal, trace_headers).await?
        };
        validate_result(&self.info, inputs.len(), &result)?;
        Ok(result)
    }
}

struct QwenHttpRequest {
    endpoint: String,
    bearer_token: String,
    body: Value,
    signal: Option<CancellationToken>,
    trace_headers: Option<EmbeddingTraceHeaders>,
}

struct QwenHttpResponse {
    status: u16,
    retry_after: Option<String>,
    body: Vec<u8>,
}

impl QwenHttpResponse {
    const fn success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

#[async_trait]
trait QwenHttpClient: Send + Sync {
    async fn post(&self, request: QwenHttpRequest) -> Result<QwenHttpResponse, ModelError>;
}

struct ReqwestQwenHttpClient {
    client: reqwest::Client,
}

impl ReqwestQwenHttpClient {
    fn new() -> Result<Self, ModelError> {
        let client = reqwest::Client::builder()
            .timeout(REMOTE_TIMEOUT)
            .build()
            .map_err(|error| {
                ModelError::internal("Unable to initialize Qwen HTTP client").with_cause(error)
            })?;
        Ok(Self { client })
    }
}

#[async_trait]
impl QwenHttpClient for ReqwestQwenHttpClient {
    async fn post(&self, request: QwenHttpRequest) -> Result<QwenHttpResponse, ModelError> {
        let future = async {
            let endpoint = request.endpoint.clone();
            let mut request_builder = self
                .client
                .post(&request.endpoint)
                .bearer_auth(&request.bearer_token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(serde_json::to_vec(&request.body).map_err(|error| {
                    ModelError::internal("unable to serialize Qwen embedding request")
                        .with_cause(error)
                })?);
            if let Some(headers) = &request.trace_headers {
                request_builder = request_builder.header("traceparent", &headers.traceparent);
                if let Some(tracestate) = &headers.tracestate {
                    request_builder = request_builder.header("tracestate", tracestate);
                }
                if let Some(baggage) = &headers.baggage {
                    request_builder = request_builder.header("baggage", baggage);
                }
            }
            let response = request_builder.send().await.map_err(|error| {
                qwen_http_error("failed to send Qwen embedding request", &endpoint, error)
            })?;
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let body = response
                .bytes()
                .await
                .map_err(|error| {
                    qwen_http_error("failed to read Qwen embedding response", &endpoint, error)
                })?
                .to_vec();
            Ok(QwenHttpResponse {
                status,
                retry_after,
                body,
            })
        };
        if let Some(signal) = request.signal {
            tokio::select! {
                result = future => result,
                () = signal.cancelled() => {
                    Err(ModelError::cancelled("embedding request was cancelled"))
                },
            }
        } else {
            future.await
        }
    }
}

#[track_caller]
fn qwen_http_error(message: &str, endpoint: &str, error: reqwest::Error) -> ModelError {
    let context = Some(format!(
        "endpoint={endpoint} timeoutMs={}",
        REMOTE_TIMEOUT.as_millis()
    ));
    let timed_out = error.is_timeout();
    let transient = !error.is_builder()
        && (timed_out || error.is_connect() || error.is_request() || error.is_body());
    let code = if timed_out {
        crate::EngineError::DEADLINE_EXCEEDED
    } else {
        crate::EngineError::INTERNAL
    };
    let error = ModelError::new(code, message, context).with_cause(error);
    if transient {
        error.transient(None)
    } else {
        error.shared()
    }
}

fn parse_response_body(
    response: &QwenHttpResponse,
    entry: QwenConfig,
) -> Result<Value, ModelError> {
    serde_json::from_slice(&response.body).map_err(|error| {
        let code = if response.success() {
            crate::EngineError::INTERNAL
        } else {
            provider_error_code(response.status)
        };
        classify_provider_failure(
            ModelError::new(
                code,
                format!("{} response was not valid JSON", model_name(entry)),
                Some(format!(
                    "model={} status={}",
                    entry.reference, response.status
                )),
            )
            .with_cause(error),
            response.status,
            response.retry_after.as_deref().and_then(retry_after_millis),
            None,
            None,
        )
    })
}

#[track_caller]
fn provider_error(entry: QwenConfig, response: &QwenHttpResponse, body: &Value) -> ModelError {
    let (code, error_type, message) = if let Some(error) = body
        .as_object()
        .and_then(|body| body.get("error"))
        .and_then(Value::as_object)
    {
        (
            error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            error
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
        )
    } else {
        (
            body.get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            "unknown",
            body.get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
        )
    };
    let retry_after = response
        .retry_after
        .as_deref()
        .and_then(retry_after_millis)
        .map_or_else(String::new, |millis| format!(" retryAfterMs={millis}"));
    classify_provider_failure(
        ModelError::new(
            provider_error_code(response.status),
            format!("{} request returned an error", model_name(entry)),
            Some(format!(
                "model={} status={}{} providerCode={} providerType={} providerMessage={}",
                entry.model, response.status, retry_after, code, error_type, message
            )),
        ),
        response.status,
        response.retry_after.as_deref().and_then(retry_after_millis),
        Some(code),
        Some(message),
    )
}

fn classify_provider_failure(
    error: ModelError,
    status: u16,
    retry_after_millis: Option<u128>,
    provider_code: Option<&str>,
    provider_message: Option<&str>,
) -> ModelError {
    let retry_after = retry_after_millis
        .map(|millis| Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX)));
    let rate_limited = status == 429
        || [provider_code, provider_message]
            .into_iter()
            .flatten()
            .any(is_rate_limit_text);
    if rate_limited {
        return error.rate_limited(retry_after);
    }
    if status == 408 || (500..=599).contains(&status) {
        return error.transient(retry_after);
    }
    if matches!(status, 401 | 403 | 404)
        || is_authentication_failure(provider_code, provider_message)
        || (status == 400 && is_permanent_model_bad_request(provider_code, provider_message))
    {
        return error.shared();
    }
    error
}

fn is_rate_limit_text(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase().replace(['_', '-'], " ");
    [
        "rate limit",
        "quota exceeded",
        "too many requests",
        "request rate increased too quickly",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

fn is_authentication_failure(provider_code: Option<&str>, provider_message: Option<&str>) -> bool {
    [provider_code, provider_message]
        .into_iter()
        .flatten()
        .any(|value| {
            let compact = value
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .map(|character| character.to_ascii_lowercase())
                .collect::<String>();
            [
                "invalidkey",
                "invalidapikey",
                "missingkey",
                "missingapikey",
                "unauthorizedkey",
                "unauthorizedapikey",
                "forbiddenkey",
                "forbiddenapikey",
            ]
            .iter()
            .any(|marker| compact.contains(marker))
        })
}

fn is_permanent_model_bad_request(
    provider_code: Option<&str>,
    provider_message: Option<&str>,
) -> bool {
    const PERMANENT_CODES: &[&str] = &[
        "invalid_model",
        "model_not_found",
        "unsupported_model",
        "invalid_dimension",
        "invalid_dimensions",
        "unsupported_dimension",
        "unsupported_dimensions",
        "dimension_out_of_range",
        "invalid_embedding_dimension",
        "unsupported_embedding_dimension",
    ];
    let normalized_code = provider_code.map(|code| {
        code.split(|character: char| !character.is_ascii_alphanumeric())
            .filter(|part| !part.is_empty())
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>()
            .join("_")
    });
    if normalized_code
        .as_deref()
        .is_some_and(|code| PERMANENT_CODES.contains(&code))
    {
        return true;
    }
    let Some(message) = provider_message.map(str::to_ascii_lowercase) else {
        return false;
    };
    let model_failure = message.contains("model")
        && [
            "invalid",
            "unsupported",
            "unknown",
            "not found",
            "does not exist",
        ]
        .iter()
        .any(|marker| message.contains(marker));
    let dimension_failure = message.contains("dimension")
        && [
            "invalid",
            "unsupported",
            "not supported",
            "out of range",
            "must",
            "should",
            "expected",
            "between",
            "only support",
        ]
        .iter()
        .any(|marker| message.contains(marker));
    model_failure || dimension_failure
}

fn provider_error_code(status: u16) -> &'static str {
    match status {
        400 | 413 | 422 => crate::EngineError::INVALID_ARGUMENT,
        401 | 403 => crate::EngineError::PERMISSION_DENIED,
        404 => crate::EngineError::NOT_FOUND,
        405 | 501 => crate::EngineError::UNSUPPORTED,
        408 | 504 => crate::EngineError::DEADLINE_EXCEEDED,
        409 | 423 | 429 => crate::EngineError::RESOURCE_BUSY,
        _ => crate::EngineError::INTERNAL,
    }
}

fn retry_after_millis(value: &str) -> Option<u128> {
    if let Ok(seconds) = value.parse::<f64>()
        && seconds.is_finite()
        && seconds >= 0.0
    {
        return format!("{:.0}", seconds * 1_000.0).parse().ok();
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(
        date.duration_since(std::time::SystemTime::now())
            .unwrap_or_default()
            .as_millis(),
    )
}

fn parse_vector(
    value: Option<&Value>,
    entry: QwenConfig,
    index: usize,
) -> Result<Vec<f32>, ModelError> {
    let values = value.and_then(Value::as_array).ok_or_else(|| {
        ModelError::new(
            crate::EngineError::INTERNAL,
            format!(
                "{} response included an invalid embedding",
                model_name(entry)
            ),
            Some(format!("model={} index={index}", entry.reference)),
        )
    })?;
    Ok(values
        .iter()
        .map(|value| value.as_f64().map_or(f32::NAN, narrow_float))
        .collect())
}

#[allow(clippy::cast_possible_truncation)]
fn narrow_float(value: f64) -> f32 {
    value as f32
}

fn collect_vectors(
    vectors: Vec<Option<Vec<f32>>>,
    entry: QwenConfig,
) -> Result<Vec<Vec<f32>>, ModelError> {
    vectors
        .into_iter()
        .enumerate()
        .map(|(index, vector)| {
            vector.ok_or_else(|| {
                ModelError::new(
                    crate::EngineError::INTERNAL,
                    "Embedding model returned a non-array vector",
                    Some(format!("model={} vectorIndex={index}", entry.reference)),
                )
            })
        })
        .collect()
}

fn json_integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        let value = value.as_f64()?;
        if !value.is_finite() || value.fract() != 0.0 {
            return None;
        }
        format!("{value:.0}").parse().ok()
    })
}

fn invalid_text_index(entry: QwenConfig, index: &str) -> ModelError {
    ModelError::new(
        crate::EngineError::INTERNAL,
        format!("{} response included an invalid index", model_name(entry)),
        Some(format!("model={} index={index}", entry.reference)),
    )
}

fn index_out_of_range(
    entry: QwenConfig,
    index: impl std::fmt::Display,
    count: usize,
) -> ModelError {
    ModelError::new(
        crate::EngineError::INTERNAL,
        format!("{} response index was out of range", model_name(entry)),
        Some(format!(
            "model={} index={index} inputCount={count}",
            entry.reference
        )),
    )
}

fn multimodal_index_out_of_range(
    entry: QwenConfig,
    index: impl std::fmt::Display,
    count: usize,
) -> ModelError {
    ModelError::new(
        crate::EngineError::INTERNAL,
        "Qwen3 VL embedding response index was out of range",
        Some(format!(
            "model={} index={index} inputCount={count}",
            entry.reference
        )),
    )
}

fn validate_multimodal_inputs(
    entry: QwenConfig,
    inputs: &[Vec<Content>],
) -> Result<(), ModelError> {
    let mut image_count = 0;
    for (index, input) in inputs.iter().enumerate() {
        let [content] = input.as_slice() else {
            return Err(ModelError::new(
                crate::EngineError::UNSUPPORTED,
                "Qwen multimodal embedding requires one text or image content per input",
                Some(format!("model={} inputIndex={index}", entry.model)),
            ));
        };
        let Content::Image(image) = content else {
            continue;
        };
        image_count += 1;
        if !matches!(
            image.format(),
            FileFormat::Jpeg | FileFormat::Png | FileFormat::Webp
        ) {
            return Err(ModelError::new(
                crate::EngineError::UNSUPPORTED,
                "Qwen3 VL embedding model does not support image format",
                Some(format!(
                    "model={} index={index} format={}",
                    entry.model,
                    image.format().as_str()
                )),
            ));
        }
    }
    if image_count > MAX_MULTIMODAL_IMAGES {
        return Err(ModelError::new(
            crate::EngineError::INVALID_ARGUMENT,
            "Qwen3 VL embedding image count exceeds model limit",
            Some(format!(
                "model={} imageCount={image_count} maxImageCount={MAX_MULTIMODAL_IMAGES}",
                entry.model
            )),
        ));
    }
    Ok(())
}

fn bytes_to_base64(bytes: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied();
        let third = chunk.get(2).copied();
        output.push(char::from(CHARS[usize::from(first >> 2)]));
        output.push(char::from(
            CHARS[usize::from(((first & 3) << 4) | second.unwrap_or(0) >> 4)],
        ));
        output.push(second.map_or('=', |second| {
            char::from(CHARS[usize::from(((second & 15) << 2) | third.unwrap_or(0) >> 6)])
        }));
        output.push(third.map_or('=', |third| char::from(CHARS[usize::from(third & 63)])));
    }
    output
}

fn model_name(entry: QwenConfig) -> &'static str {
    match entry.model {
        "text-embedding-v4" => "Qwen text-embedding-v4",
        "qwen3.7-text-embedding" => "Qwen3.7 text embedding",
        _ => "Qwen3 VL embedding",
    }
}

#[cfg(test)]
mod tests;
