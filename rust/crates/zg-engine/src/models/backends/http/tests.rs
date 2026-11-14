use std::{collections::HashMap, fmt::Write, time::Duration};

use futures_util::FutureExt;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::HttpEmbeddingModel;
use crate::{
    EngineError,
    domain::{
        Content,
        model::{EmbeddingMetric, EmbeddingPurpose, HttpAuth, HttpEmbeddingConfig, ModelConfig},
    },
    models::spi::{EmbeddingModel, EmbeddingOptions, EmbeddingTraceHeaders, ModelError},
};

struct CapturedRequest {
    target: String,
    headers: HashMap<String, String>,
    body: Value,
}

impl CapturedRequest {
    async fn read(stream: &mut TcpStream) -> Self {
        let mut reader = BufReader::new(stream);
        let mut target = String::new();
        reader.read_line(&mut target).await.expect("request line");
        let mut headers = HashMap::new();
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).await.expect("header"), 0);
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.split_once(':').expect("header field");
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        let length = headers["content-length"].parse().expect("content length");
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes).await.expect("request body");
        Self {
            target: target.trim_end().to_owned(),
            headers,
            body: serde_json::from_slice(&bytes).expect("request JSON"),
        }
    }
}

enum Reply {
    Complete(u16, Vec<(String, String)>, String),
    Hold { send_headers: bool },
}

struct MockServer {
    endpoint: String,
    request: Option<oneshot::Receiver<CapturedRequest>>,
    task: JoinHandle<()>,
}

impl MockServer {
    async fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let endpoint = format!(
            "http://{}/v1/embeddings",
            listener.local_addr().expect("addr")
        );
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let request = CapturedRequest::read(&mut stream).await;
            match reply {
                Reply::Complete(status, headers, body) => {
                    let mut response = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (name, value) in headers {
                        write!(response, "{name}: {value}\r\n").expect("write response header");
                    }
                    response.push_str("\r\n");
                    response.push_str(&body);
                    stream
                        .write_all(response.as_bytes())
                        .await
                        .expect("write response");
                    let _ = sender.send(request);
                }
                Reply::Hold { send_headers } => {
                    if send_headers {
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n")
                            .await
                            .expect("write headers");
                    }
                    let _ = sender.send(request);
                    std::future::pending::<()>().await;
                }
            }
        });
        Self {
            endpoint,
            request: Some(receiver),
            task,
        }
    }

    fn model(&self, config: HttpEmbeddingConfig) -> HttpEmbeddingModel {
        HttpEmbeddingModel::new("http/alias", options(&self.endpoint, config)).expect("model")
    }

    async fn request(&mut self) -> CapturedRequest {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.request.take().expect("receiver"),
        )
        .await
        .expect("request deadline")
        .expect("captured request")
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn descriptor() -> HttpEmbeddingConfig {
    HttpEmbeddingConfig {
        model: "uncataloged-fixture-model".into(),
        dimension: 4,
        metric: EmbeddingMetric::Cosine,
        max_input_tokens: 128,
        max_batch_size: 2,
        timeout_ms: 5_000,
        auth: HttpAuth::None,
        api_key_env: None,
        query_prefix: "query:".into(),
        document_prefix: "document:".into(),
        send_dimensions: false,
        send_encoding_format: false,
        single_input: false,
    }
}

fn options(endpoint: &str, http: HttpEmbeddingConfig) -> ModelConfig {
    ModelConfig {
        endpoint: Some(endpoint.to_owned()),
        http: Some(http),
        api_key: Some("generic-key-must-never-be-used".into()),
        ..ModelConfig::default()
    }
}

fn inputs() -> Vec<Vec<Content>> {
    vec![
        vec![Content::Text("first".into())],
        vec![Content::Code("second".into())],
    ]
}

fn success() -> String {
    json!({"data": [
        {"index": 1, "embedding": [1, 2, 3, 4]},
        {"index": 0, "embedding": [0, 0, 0, 0]}
    ]})
    .to_string()
}

async fn response_error(reply: Reply) -> ModelError {
    let server = MockServer::start(reply).await;
    server
        .model(descriptor())
        .embed(&inputs(), EmbeddingOptions::default())
        .await
        .expect_err("HTTP response failure")
}

#[tokio::test]
async fn sends_descriptor_model_and_prefixes_without_auth_or_optional_fields() {
    for (purpose, prefix) in [
        (EmbeddingPurpose::Document, "document:"),
        (EmbeddingPurpose::Query, "query:"),
    ] {
        let mut server = MockServer::start(Reply::Complete(200, vec![], success())).await;
        let model = server.model(descriptor());
        let result = model
            .embed(
                &inputs(),
                EmbeddingOptions {
                    purpose,
                    ..EmbeddingOptions::default()
                },
            )
            .await
            .expect("embedding");
        assert_eq!(result.vectors, [[0.0; 4], [1.0, 2.0, 3.0, 4.0]]);
        assert!(result.truncated.is_empty());
        let request = server.request().await;
        assert_eq!(request.target, "POST /v1/embeddings HTTP/1.1");
        assert!(!request.headers.contains_key("authorization"));
        assert_eq!(
            request.body,
            json!({"model": "uncataloged-fixture-model", "input": [format!("{prefix}first"), format!("{prefix}second")]})
        );
        assert_eq!(model.info().model().provider(), "http");
        assert_eq!(model.info().model().name(), "alias");
        assert_eq!(model.info().model().max_batch_size(), 2);
        assert_eq!(model.info().dimension(), 4);
        assert_eq!(model.info().metric(), EmbeddingMetric::Cosine);
        assert_eq!(model.info().max_input_tokens(), Some(119));
    }
}

#[tokio::test]
async fn single_input_optional_fields_and_trace_headers_are_explicit() {
    for (dimensions, encoding_format) in [(true, false), (false, true), (true, true)] {
        let body = json!({"data": [{"index": 0, "embedding": [0, 1, 2, 3]}]}).to_string();
        let mut server = MockServer::start(Reply::Complete(200, vec![], body)).await;
        let mut config = descriptor();
        config.single_input = true;
        config.max_batch_size = 1;
        config.send_dimensions = dimensions;
        config.send_encoding_format = encoding_format;
        config.metric = EmbeddingMetric::DotProduct;
        let model = server.model(config);
        model
            .embed(
                &[vec![
                    Content::Text("part one".into()),
                    Content::Code("part two".into()),
                ]],
                EmbeddingOptions {
                    trace_headers: Some(EmbeddingTraceHeaders {
                        traceparent: "trace-parent".into(),
                        tracestate: Some("state=value".into()),
                        baggage: Some("bag=value".into()),
                    }),
                    ..EmbeddingOptions::default()
                },
            )
            .await
            .expect("embedding");
        let request = server.request().await;
        let mut expected =
            json!({"model": "uncataloged-fixture-model", "input": "document:part one\npart two"});
        if dimensions {
            expected["dimensions"] = json!(4);
        }
        if encoding_format {
            expected["encoding_format"] = json!("float");
        }
        assert_eq!(request.body, expected);
        for (name, expected) in [
            ("traceparent", "trace-parent"),
            ("tracestate", "state=value"),
            ("baggage", "bag=value"),
        ] {
            assert_eq!(
                request.headers.get(name).map(String::as_str),
                Some(expected)
            );
        }
        assert_eq!(model.info().metric(), EmbeddingMetric::DotProduct);
    }
}

#[test]
fn bearer_credentials_are_validated_and_redacted() {
    const CHILD: &str = "ZG_HTTP_BACKEND_AUTH_TEST_CHILD";
    const KEY: &str = "ZG_HTTP_BACKEND_AUTH_TEST_KEY";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "models::backends::http::tests::bearer_credentials_are_validated_and_redacted",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(KEY, "named-fixture-secret")
            .env("ZG_HTTP_BACKEND_AUTH_TEST_EMPTY", "")
            .env("ZG_HTTP_BACKEND_AUTH_TEST_INVALID", "invalid\nheader")
            .env_remove("ZG_HTTP_BACKEND_AUTH_TEST_MISSING")
            .output()
            .expect("isolated auth test");
        assert!(
            result.status.success(),
            "isolated auth test failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let endpoint = "http://localhost/v1/embeddings";
    let mut config = descriptor();
    config.auth = HttpAuth::Bearer;
    config.api_key_env = Some(KEY.into());
    let model = HttpEmbeddingModel::new("http/alias", options(endpoint, config.clone()))
        .expect("named credentials");
    assert!(!format!("{model:?}").contains("named-fixture-secret"));
    for env in [
        "ZG_HTTP_BACKEND_AUTH_TEST_EMPTY",
        "ZG_HTTP_BACKEND_AUTH_TEST_INVALID",
        "ZG_HTTP_BACKEND_AUTH_TEST_MISSING",
    ] {
        config.api_key_env = Some(env.into());
        let error = HttpEmbeddingModel::new("http/alias", options(endpoint, config.clone()))
            .expect_err("invalid bearer key");
        assert_eq!(error.code(), EngineError::PERMISSION_DENIED);
        assert!(error.should_fail_fast());
        assert!(!error.is_retryable());
        assert!(!format!("{error:?}").contains("generic-key-must-never-be-used"));
        assert!(!format!("{error:?}").contains("invalid\nheader"));
    }
}

#[tokio::test]
async fn rejects_malformed_count_indices_dimensions_and_nonfinite_values() {
    let invalid = [
        json!({"data": []}).to_string(),
        json!({"data": [{"index": 0, "embedding": [0, 0, 0, 0]}]}).to_string(),
        json!({"data": [{"index": 0, "embedding": [0, 0, 0, 0]}, {"index": 0, "embedding": [0, 0, 0, 0]}]}).to_string(),
        json!({"data": [{"index": 2, "embedding": [0, 0, 0, 0]}, {"index": 1, "embedding": [0, 0, 0, 0]}]}).to_string(),
        json!({"data": [{"index": -1, "embedding": [0, 0, 0, 0]}, {"index": 1, "embedding": [0, 0, 0, 0]}]}).to_string(),
        json!({"data": [{"embedding": [0, 0, 0, 0]}, {"index": 1, "embedding": [0, 0, 0, 0]}]}).to_string(),
        json!({"data": [{"index": 0.5, "embedding": [0, 0, 0, 0]}, {"index": 1, "embedding": [0, 0, 0, 0]}]}).to_string(),
        json!({"data": [{"index": 0, "embedding": [0, 0, 0]}, {"index": 1, "embedding": [0, 0, 0, 0]}]}).to_string(),
        json!({"data": [{"index": 0, "embedding": [1e100, 0, 0, 0]}, {"index": 1, "embedding": [0, 0, 0, 0]}]}).to_string(),
        json!({"data": [{"index": 0, "embedding": ["secret-response", 0, 0, 0]}, {"index": 1, "embedding": [0, 0, 0, 0]}]}).to_string(),
        "{\"data\":[{\"index\":0,\"embedding\":[NaN,0,0,0]}]}".into(),
        "{\"data\":[{\"index\":0,\"embedding\":[1e999,0,0,0]}]}".into(),
        "secret-response-not-json".into(),
        json!({"error": {"message": "secret-response"}}).to_string(),
    ];
    for body in invalid {
        let error = response_error(Reply::Complete(200, vec![], body)).await;
        assert!(!error.is_retryable());
        assert!(error.should_fail_fast());
        assert!(!format!("{error:?}").contains("secret-response"));
    }
}

#[tokio::test]
async fn classifies_status_and_retry_after_without_exposing_error_bodies() {
    for (status, retryable, rate_limited) in [
        (429, true, true),
        (408, true, false),
        (500, true, false),
        (503, true, false),
        (401, false, false),
        (403, false, false),
        (404, false, false),
        (400, false, false),
        (422, false, false),
    ] {
        let error = response_error(Reply::Complete(
            status,
            vec![("Retry-After".into(), "1.5".into())],
            "secret-provider-error".into(),
        ))
        .await;
        assert_eq!(error.is_retryable(), retryable, "status {status}");
        assert_eq!(error.is_rate_limited(), rate_limited, "status {status}");
        assert_eq!(
            error.retry_after(),
            retryable.then_some(Duration::from_millis(1500))
        );
        assert!(!format!("{error:?}").contains("secret-provider-error"));
        if matches!(status, 401 | 403 | 404) {
            assert!(error.should_fail_fast());
        }
    }
    for (header, expected) in [
        ("0", Some(Duration::ZERO)),
        ("Thu, 01 Jan 1970 00:00:00 GMT", Some(Duration::ZERO)),
        ("invalid", None),
        ("NaN", None),
        ("-1", None),
        ("1e300", Some(Duration::from_millis(u64::from(u32::MAX)))),
    ] {
        let error = response_error(Reply::Complete(
            429,
            vec![("Retry-After".into(), header.into())],
            String::new(),
        ))
        .await;
        assert_eq!(error.retry_after(), expected, "header {header}");
    }
}

#[tokio::test]
async fn does_not_follow_redirects() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirect destination");
    let error = response_error(Reply::Complete(
        307,
        vec![(
            "Location".into(),
            format!("http://{}/private", listener.local_addr().expect("address")),
        )],
        String::new(),
    ))
    .await;
    assert!(!error.is_retryable());
    assert!(error.should_fail_fast());
    assert!(listener.accept().now_or_never().is_none());
}

#[tokio::test]
async fn cancellation_and_deadline_cover_waiting_for_headers_and_body() {
    for cancel in [true, false] {
        for send_headers in [false, true] {
            let mut server = MockServer::start(Reply::Hold { send_headers }).await;
            let mut config = descriptor();
            if !cancel {
                config.timeout_ms = 50;
            }
            let model = server.model(config);
            let signal = CancellationToken::new();
            let cancelled = signal.clone();
            let task = tokio::spawn(async move {
                model
                    .embed(
                        &inputs(),
                        EmbeddingOptions {
                            signal: cancel.then_some(cancelled),
                            ..EmbeddingOptions::default()
                        },
                    )
                    .await
            });
            if cancel {
                server.request().await;
                signal.cancel();
            }
            let error = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("request deadline")
                .expect("join")
                .expect_err("interrupted");
            let expected = if cancel {
                EngineError::CANCELLED
            } else {
                EngineError::DEADLINE_EXCEEDED
            };
            assert_eq!(
                error.code(),
                expected,
                "cancel={cancel} send_headers={send_headers}"
            );
            assert_eq!(error.is_retryable(), !cancel);
            assert!(error.should_fail_fast());
        }
    }
}

#[tokio::test]
async fn cancelled_and_oversized_batches_do_not_send_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!(
        "http://{}/v1/embeddings",
        listener.local_addr().expect("address")
    );
    let model =
        HttpEmbeddingModel::new("http/alias", options(&endpoint, descriptor())).expect("model");
    let signal = CancellationToken::new();
    signal.cancel();
    let error = model
        .embed(
            &inputs(),
            EmbeddingOptions {
                signal: Some(signal),
                ..EmbeddingOptions::default()
            },
        )
        .await
        .expect_err("cancelled");
    assert_eq!(error.code(), EngineError::CANCELLED);
    let oversized = vec![vec![Content::Text("text".into())]; 3];
    let error = model
        .embed(&oversized, EmbeddingOptions::default())
        .await
        .expect_err("batch limit");
    assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
    assert!(listener.accept().now_or_never().is_none());
}

#[tokio::test]
async fn preserves_raw_text_at_the_reserved_input_limit() {
    let mut server = MockServer::start(Reply::Complete(200, vec![], success())).await;
    let model = server.model(descriptor());
    let text = "文".repeat(model.info().max_input_tokens().expect("input budget"));
    let inputs = vec![
        vec![Content::Text(text.clone())],
        vec![Content::Code(text.clone())],
    ];
    let result = model
        .embed(&inputs, EmbeddingOptions::default())
        .await
        .expect("embedding");
    assert!(result.truncated.is_empty());
    let request = server.request().await;
    assert_eq!(
        request.body["input"],
        json!([format!("document:{text}"), format!("document:{text}")])
    );
}

#[test]
fn validates_reference_descriptor_presence_and_endpoint_before_network() {
    let endpoint = "http://localhost/v1/embeddings";
    for reference in ["qwen/alias", "http/", "http/   "] {
        assert!(HttpEmbeddingModel::new(reference, options(endpoint, descriptor())).is_err());
    }
    let mut missing = options(endpoint, descriptor());
    missing.http = None;
    assert!(HttpEmbeddingModel::new("http/alias", missing).is_err());
    for endpoint in [
        "",
        "not-a-url",
        "file:///etc/passwd",
        "http://user:secret@localhost/embed",
        "http://localhost/embed?key=secret",
        "http://localhost/embed#secret",
    ] {
        let error = HttpEmbeddingModel::new("http/alias", options(endpoint, descriptor()))
            .expect_err("invalid endpoint");
        assert!(!format!("{error:?}").contains("secret"));
        assert!(error.should_fail_fast());
    }
}
