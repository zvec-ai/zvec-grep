use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::domain::{Content, FileFormat};

use super::{
    catalog::QwenConfig,
    spi::{
        CreateEmbeddingModelOptions, EmbeddingInput, EmbeddingInputKind, EmbeddingModel,
        EmbeddingModelInfo, EmbeddingModelLimits, EmbeddingOptions, EmbeddingResult,
        EmbeddingTraceHeaders, ModelError, validate_inputs, validate_result,
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
    pub(crate) fn new(
        entry: QwenConfig,
        options: CreateEmbeddingModelOptions,
    ) -> Result<Self, ModelError> {
        Self::with_http(entry, options, Arc::new(ReqwestQwenHttpClient::new()?))
    }

    fn with_http(
        entry: QwenConfig,
        options: CreateEmbeddingModelOptions,
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
            ));
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
            ));
        }
        let input_kinds = if entry.kind == "multimodal" {
            vec![EmbeddingInputKind::Text, EmbeddingInputKind::Image]
        } else {
            vec![EmbeddingInputKind::Text]
        };
        Ok(Self {
            entry,
            info: EmbeddingModelInfo {
                reference: entry.reference.to_owned(),
                provider: entry.provider.to_owned(),
                name: entry.model.to_owned(),
                dimension: entry.dimension,
                metric: entry.metric,
                endpoint: Some(endpoint.clone()),
                default_concurrency: None,
                input_kinds,
                limits: EmbeddingModelLimits {
                    max_batch_size: entry.max_batch_size,
                    max_input_tokens: Some(entry.max_input_tokens),
                    max_image_bytes: entry.max_image_bytes,
                },
            },
            api_key,
            endpoint,
            http,
        })
    }

    async fn embed_text(
        &self,
        inputs: &[EmbeddingInput],
        signal: Option<CancellationToken>,
        trace_headers: Option<EmbeddingTraceHeaders>,
    ) -> Result<EmbeddingResult, ModelError> {
        let texts = inputs
            .iter()
            .map(EmbeddingInput::to_text)
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
        inputs: &[EmbeddingInput],
        signal: Option<CancellationToken>,
        trace_headers: Option<EmbeddingTraceHeaders>,
    ) -> Result<EmbeddingResult, ModelError> {
        validate_multimodal_inputs(self.entry, inputs)?;
        let request_contents = inputs
            .iter()
            .map(|input| match input.contents.as_slice() {
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

    async fn embed(
        &self,
        inputs: &[EmbeddingInput],
        options: EmbeddingOptions,
    ) -> Result<EmbeddingResult, ModelError> {
        validate_inputs(&self.info, inputs)?;
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
    if error.is_timeout() {
        ModelError::new(crate::EngineError::DEADLINE_EXCEEDED, message, context).with_cause(error)
    } else {
        ModelError::new(crate::EngineError::INTERNAL, message, context).with_cause(error)
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
        ModelError::new(
            code,
            format!("{} response was not valid JSON", model_name(entry)),
            Some(format!(
                "model={} status={}",
                entry.reference, response.status
            )),
        )
        .with_cause(error)
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
    ModelError::new(
        provider_error_code(response.status),
        format!("{} request returned an error", model_name(entry)),
        Some(format!(
            "model={} status={}{} providerCode={} providerType={} providerMessage={}",
            entry.model, response.status, retry_after, code, error_type, message
        )),
    )
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
    inputs: &[EmbeddingInput],
) -> Result<(), ModelError> {
    let mut image_count = 0;
    for (index, input) in inputs.iter().enumerate() {
        let [content] = input.contents.as_slice() else {
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
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{
        domain::{ImageContent, TableContent},
        models::spi::EmbeddingMetric,
    };

    struct MockHttp {
        response: Mutex<Option<QwenHttpResponse>>,
        requests: Mutex<Vec<(Value, Option<EmbeddingTraceHeaders>)>>,
    }

    #[async_trait]
    impl QwenHttpClient for MockHttp {
        async fn post(&self, request: QwenHttpRequest) -> Result<QwenHttpResponse, ModelError> {
            assert_eq!(request.endpoint, "https://example.test/embed");
            assert_eq!(request.bearer_token, "secret");
            self.requests
                .lock()
                .expect("requests lock")
                .push((request.body, request.trace_headers));
            self.response
                .lock()
                .expect("response lock")
                .take()
                .ok_or_else(|| ModelError::internal("mock Qwen response is missing"))
        }
    }

    fn config(kind: &'static str, model: &'static str, dimension: usize) -> QwenConfig {
        QwenConfig {
            kind,
            reference: "qwen/test",
            provider: "qwen",
            model,
            dimension,
            metric: EmbeddingMetric::Cosine,
            default_endpoint: "https://default.test/embed",
            max_batch_size: 20,
            max_input_tokens: 512,
            max_image_bytes: Some(1024),
        }
    }

    fn options() -> CreateEmbeddingModelOptions {
        CreateEmbeddingModelOptions {
            api_key: Some(" secret ".to_owned()),
            endpoint: Some(" https://example.test/embed ".to_owned()),
            ..CreateEmbeddingModelOptions::default()
        }
    }

    #[tokio::test]
    async fn text_request_and_index_order_match_main() {
        let http = Arc::new(MockHttp {
            response: Mutex::new(Some(QwenHttpResponse {
                status: 200,
                retry_after: None,
                body: serde_json::to_vec(&json!({
                    "data": [
                        { "index": 1, "embedding": [4.0, 5.0, 6.0] },
                        { "index": 0, "embedding": [1.0, 2.0, 3.0] }
                    ]
                }))
                .expect("fixture JSON"),
            })),
            requests: Mutex::new(Vec::new()),
        });
        let model = QwenEmbeddingModel::with_http(
            config("text", "text-embedding-v4", 3),
            options(),
            http.clone(),
        )
        .expect("model");
        let result = model
            .embed(
                &[
                    EmbeddingInput::new(vec![
                        Content::Text("one".to_owned()),
                        Content::Text("part".to_owned()),
                    ]),
                    EmbeddingInput::text("two".to_owned()),
                ],
                EmbeddingOptions {
                    trace_headers: Some(EmbeddingTraceHeaders {
                        traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                            .to_owned(),
                        tracestate: Some("vendor=value".to_owned()),
                        baggage: Some("tenant=search".to_owned()),
                    }),
                    ..EmbeddingOptions::default()
                },
            )
            .await
            .expect("embedding");
        assert_eq!(result.vectors, [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]);
        assert_eq!(
            http.requests.lock().expect("requests lock")[0].0,
            json!({
                "model": "text-embedding-v4",
                "input": ["one\npart", "two"],
                "dimensions": 3,
                "encoding_format": "float"
            })
        );
        assert_eq!(
            http.requests.lock().expect("requests lock")[0].1,
            Some(EmbeddingTraceHeaders {
                traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_owned(),
                tracestate: Some("vendor=value".to_owned()),
                baggage: Some("tenant=search".to_owned()),
            })
        );
    }

    #[tokio::test]
    async fn multimodal_request_preserves_content_and_rejects_unsupported_inputs() {
        let http = Arc::new(MockHttp {
            response: Mutex::new(Some(QwenHttpResponse {
                status: 200,
                retry_after: None,
                body: serde_json::to_vec(&json!({
                    "output": { "embeddings": [
                        { "text_index": 0, "embedding": [1.0, 0.0] },
                        { "index": 1, "embedding": [0.0, 1.0] }
                    ] }
                }))
                .expect("fixture JSON"),
            })),
            requests: Mutex::new(Vec::new()),
        });
        let model = QwenEmbeddingModel::with_http(
            config("multimodal", "qwen3-vl-embedding", 2),
            options(),
            http.clone(),
        )
        .expect("model");
        let result = model
            .embed(
                &[
                    EmbeddingInput::text("query".to_owned()),
                    EmbeddingInput::new(vec![Content::Image(
                        ImageContent::new(vec![1, 2, 3], FileFormat::Png).expect("image"),
                    )]),
                ],
                EmbeddingOptions::default(),
            )
            .await
            .expect("embedding");
        assert_eq!(result.vectors, [[1.0, 0.0], [0.0, 1.0]]);
        assert_eq!(
            http.requests.lock().expect("requests lock")[0].0["input"]["contents"][1]["image"],
            "AQID"
        );

        for content in [
            Content::Image(ImageContent::new(vec![1], FileFormat::Gif).expect("image")),
            Content::Image(ImageContent::new(vec![1], FileFormat::Svg).expect("image")),
            Content::Table(TableContent {
                row_count: 0,
                column_count: 0,
                cells: Vec::new(),
            }),
        ] {
            let error = model
                .embed(
                    &[
                        EmbeddingInput::text("query".to_owned()),
                        EmbeddingInput::new(vec![content]),
                    ],
                    EmbeddingOptions::default(),
                )
                .await
                .expect_err("unsupported content must be rejected before dispatch");
            assert_eq!(error.code(), crate::EngineError::UNSUPPORTED);
        }
        for parts in [
            vec![
                Content::Text("first".to_owned()),
                Content::Text("second".to_owned()),
            ],
            vec![
                Content::Text("query".to_owned()),
                Content::Image(ImageContent::new(vec![1], FileFormat::Png).expect("image")),
            ],
        ] {
            let error = model
                .embed(&[EmbeddingInput::new(parts)], EmbeddingOptions::default())
                .await
                .expect_err("composed multimodal inputs are unsupported");
            assert_eq!(error.code(), crate::EngineError::UNSUPPORTED);
        }
        assert_eq!(http.requests.lock().expect("requests lock").len(), 1);
    }

    #[tokio::test]
    async fn invalid_json_and_provider_errors_match_main() {
        let invalid_json = Arc::new(MockHttp {
            response: Mutex::new(Some(QwenHttpResponse {
                status: 502,
                retry_after: None,
                body: b"not json".to_vec(),
            })),
            requests: Mutex::new(Vec::new()),
        });
        let model = QwenEmbeddingModel::with_http(
            config("text", "text-embedding-v4", 3),
            options(),
            invalid_json,
        )
        .expect("model");
        let error = model
            .embed(
                &[EmbeddingInput::text("one".to_owned())],
                EmbeddingOptions::default(),
            )
            .await
            .expect_err("invalid JSON");
        assert_eq!(error.code(), crate::EngineError::INTERNAL);

        let invalid_provider_body = QwenHttpResponse {
            status: 429,
            retry_after: Some("1".to_owned()),
            body: b"rate limited".to_vec(),
        };
        let error = parse_response_body(
            &invalid_provider_body,
            config("text", "text-embedding-v4", 3),
        )
        .expect_err("non-JSON provider error");
        assert_eq!(error.code(), crate::EngineError::RESOURCE_BUSY);

        let provider_error_response = Arc::new(MockHttp {
            response: Mutex::new(Some(QwenHttpResponse {
                status: 429,
                retry_after: Some("1.5".to_owned()),
                body: serde_json::to_vec(&json!({
                    "error": {
                        "code": "rate_limit",
                        "type": "throttled",
                        "message": "slow down"
                    }
                }))
                .expect("fixture JSON"),
            })),
            requests: Mutex::new(Vec::new()),
        });
        let model = QwenEmbeddingModel::with_http(
            config("text", "text-embedding-v4", 3),
            options(),
            provider_error_response,
        )
        .expect("model");
        let error = model
            .embed(
                &[EmbeddingInput::text("one".to_owned())],
                EmbeddingOptions::default(),
            )
            .await
            .expect_err("provider error");
        assert_eq!(error.code(), crate::EngineError::RESOURCE_BUSY);
        let context = error.context().expect("provider context");
        assert!(context.contains("status=429 retryAfterMs=1500"));
        assert!(context.contains("providerCode=rate_limit"));
        assert!(context.contains("providerType=throttled"));
        assert!(context.contains("providerMessage=slow down"));
        assert!(!context.contains("secret"));

        assert_eq!(
            provider_error_code(400),
            crate::EngineError::INVALID_ARGUMENT
        );
        assert_eq!(
            provider_error_code(401),
            crate::EngineError::PERMISSION_DENIED
        );
        assert_eq!(provider_error_code(404), crate::EngineError::NOT_FOUND);
        assert_eq!(provider_error_code(405), crate::EngineError::UNSUPPORTED);
        assert_eq!(
            provider_error_code(408),
            crate::EngineError::DEADLINE_EXCEEDED
        );
        assert_eq!(provider_error_code(429), crate::EngineError::RESOURCE_BUSY);
        assert_eq!(provider_error_code(500), crate::EngineError::INTERNAL);
    }

    #[test]
    fn requires_api_key_and_keeps_catalog_endpoint() {
        let error = QwenEmbeddingModel::new(
            config("text", "qwen3.7-text-embedding", 3),
            CreateEmbeddingModelOptions::default(),
        )
        .err()
        .expect("missing API key");
        assert_eq!(error.code(), crate::EngineError::PERMISSION_DENIED);
    }
}
