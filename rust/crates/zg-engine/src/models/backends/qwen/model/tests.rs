use std::sync::{Arc, Mutex};

use super::*;
use crate::domain::{ImageContent, TableContent, model::Metric};

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
        metric: Metric::Cosine,
        default_endpoint: "https://default.test/embed",
        max_batch_size: 20,
        max_input_tokens: 512,
        max_image_bytes: Some(1024),
    }
}

fn options() -> ModelConfig {
    ModelConfig {
        api_key: Some(" secret ".to_owned()),
        endpoint: Some(" https://example.test/embed ".to_owned()),
        ..ModelConfig::default()
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
                vec![
                    Content::Text("one".to_owned()),
                    Content::Text("part".to_owned()),
                ],
                vec![Content::Text("two".to_owned())],
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
async fn code_is_sent_as_text_and_cancelled_before_http_dispatch() {
    for kind in ["text", "multimodal"] {
        let body = if kind == "text" {
            json!({"data": [{"index": 0, "embedding": [1.0, 0.0]}]})
        } else {
            json!({"output": {"embeddings": [{"text_index": 0, "embedding": [1.0, 0.0]}]}})
        };
        let http = Arc::new(MockHttp {
            response: Mutex::new(Some(QwenHttpResponse {
                status: 200,
                retry_after: None,
                body: serde_json::to_vec(&body).expect("fixture response"),
            })),
            requests: Mutex::new(Vec::new()),
        });
        let model =
            QwenEmbeddingModel::with_http(config(kind, "code-test", 2), options(), http.clone())
                .expect("model");
        let input = [vec![Content::Code("fn example() {}".into())]];
        let signal = CancellationToken::new();
        signal.cancel();
        let error = model
            .embed(
                &input,
                EmbeddingOptions {
                    signal: Some(signal),
                    ..EmbeddingOptions::default()
                },
            )
            .await
            .expect_err("cancelled code request");
        assert_eq!(error.code(), crate::EngineError::CANCELLED);
        assert!(http.requests.lock().expect("requests lock").is_empty());

        let result = model
            .embed(&input, EmbeddingOptions::default())
            .await
            .expect("code embedding");
        assert_eq!(result.vectors, [[1.0, 0.0]]);
        let requests = http.requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 1);
        let expected_input = if kind == "text" {
            json!(["fn example() {}"])
        } else {
            json!({"contents": [{"text": "fn example() {}"}]})
        };
        assert_eq!(requests[0].0["input"], expected_input);
    }
}

#[tokio::test]
async fn text_backend_rejects_images_before_http_dispatch() {
    let http = Arc::new(MockHttp {
        response: Mutex::new(None),
        requests: Mutex::new(Vec::new()),
    });
    let model = QwenEmbeddingModel::with_http(
        config("text", "text-embedding-v4", 2),
        options(),
        http.clone(),
    )
    .expect("model");
    let image = ImageContent::new(vec![1], FileFormat::Png).expect("image");
    let error = model
        .embed(&[vec![Content::Image(image)]], EmbeddingOptions::default())
        .await
        .expect_err("text backend must reject images");
    assert_eq!(error.code(), crate::EngineError::UNSUPPORTED);
    assert!(http.requests.lock().expect("requests lock").is_empty());
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
                vec![Content::Text("query".to_owned())],
                vec![Content::Image(
                    ImageContent::new(vec![1, 2, 3], FileFormat::Png).expect("image"),
                )],
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
                &[vec![Content::Text("query".to_owned())], vec![content]],
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
            .embed(&[parts], EmbeddingOptions::default())
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
            &[vec![Content::Text("one".to_owned())]],
            EmbeddingOptions::default(),
        )
        .await
        .expect_err("invalid JSON");
    assert_eq!(error.code(), crate::EngineError::INTERNAL);
    assert!(error.is_retryable());
    assert!(error.should_fail_fast());

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
    assert!(error.is_rate_limited());
    assert_eq!(error.retry_after(), Some(Duration::from_secs(1)));

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
            &[vec![Content::Text("one".to_owned())]],
            EmbeddingOptions::default(),
        )
        .await
        .expect_err("provider error");
    assert_eq!(error.code(), crate::EngineError::RESOURCE_BUSY);
    assert!(error.is_rate_limited());
    assert!(error.should_fail_fast());
    assert_eq!(error.retry_after(), Some(Duration::from_millis(1_500)));
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
fn provider_failures_expose_structured_retry_and_failure_scope() {
    let entry = config("text", "text-embedding-v4", 3);
    let response = |status| QwenHttpResponse {
        status,
        retry_after: Some("0".to_owned()),
        body: Vec::new(),
    };
    let body = |code: &str, message: &str| {
        json!({
            "error": {
                "code": code,
                "type": "fixture",
                "message": message,
            }
        })
    };

    for status in [408, 500, 503] {
        let response = response(status);
        let error = provider_error(entry, &response, &body("temporary", "try again"));
        assert!(error.is_retryable(), "status={status}");
        assert!(!error.is_rate_limited(), "status={status}");
        assert!(error.should_fail_fast(), "status={status}");
        assert_eq!(error.retry_after(), Some(Duration::ZERO));
    }

    for status in [401, 403, 404] {
        let response = response(status);
        let error = provider_error(entry, &response, &body("denied", "configuration failure"));
        assert!(!error.is_retryable(), "status={status}");
        assert!(error.should_fail_fast(), "status={status}");
    }

    for (code, message) in [
        ("InvalidApiKey", "request rejected"),
        ("bad_request", "unauthorized API key"),
    ] {
        let authentication = response(400);
        let error = provider_error(entry, &authentication, &body(code, message));
        assert!(!error.is_retryable(), "code={code}");
        assert!(error.should_fail_fast(), "code={code}");
    }

    let permanent = response(400);
    let error = provider_error(
        entry,
        &permanent,
        &body("INVALID--MODEL", "model does not exist"),
    );
    assert!(!error.is_retryable());
    assert!(error.should_fail_fast());

    let request_specific = response(400);
    let error = provider_error(
        entry,
        &request_specific,
        &body("invalid_input", "input is too long"),
    );
    assert!(!error.is_retryable());
    assert!(!error.should_fail_fast());
}

#[test]
fn non_json_http_errors_report_status_without_echoing_the_response_body() {
    let entry = config("text", "qwen3.7-text-embedding", 3);
    for body in [b"".as_slice(), b"<html>sensitive-provider-body</html>"] {
        let error = parse_response_body(
            &QwenHttpResponse {
                status: 404,
                retry_after: None,
                body: body.to_vec(),
            },
            entry,
        )
        .expect_err("missing embedding route");
        assert_eq!(error.code(), crate::EngineError::NOT_FOUND);
        assert!(error.to_string().contains("request returned HTTP 404"));
        assert!(error.context().expect("context").contains("--endpoint"));
        assert!(error.should_fail_fast());
        assert!(!error.is_retryable());
        assert!(error.cause().is_none());
        assert!(
            !error
                .into_engine_error()
                .message()
                .contains("sensitive-provider-body")
        );
    }

    let error = parse_response_body(
        &QwenHttpResponse {
            status: 200,
            retry_after: None,
            body: Vec::new(),
        },
        entry,
    )
    .expect_err("invalid successful response");
    assert_eq!(error.code(), crate::EngineError::INTERNAL);
    assert!(error.to_string().contains("response was not valid JSON"));
    assert!(error.cause().is_some());
}

#[tokio::test]
async fn json_http_errors_include_endpoint_hint_only_for_not_found() {
    for body in [
        json!({
            "error": {
                "code": "DeploymentNotFound",
                "type": "not_found_error",
                "message": "Unknown deployment",
            }
        }),
        json!({"code": "DeploymentNotFound", "message": "Unknown deployment"}),
    ] {
        for status in [400, 404] {
            let http = Arc::new(MockHttp {
                response: Mutex::new(Some(QwenHttpResponse {
                    status,
                    retry_after: None,
                    body: serde_json::to_vec(&body).expect("fixture JSON"),
                })),
                requests: Mutex::new(Vec::new()),
            });
            let model = QwenEmbeddingModel::with_http(
                config("text", "qwen3.7-text-embedding", 3),
                options(),
                http,
            )
            .expect("model");
            let error = model
                .embed(
                    &[vec![Content::Text("one".to_owned())]],
                    EmbeddingOptions::default(),
                )
                .await
                .expect_err("provider error");
            assert_eq!(
                error.code(),
                if status == 404 {
                    crate::EngineError::NOT_FOUND
                } else {
                    crate::EngineError::INVALID_ARGUMENT
                }
            );
            let context = error.context().expect("provider context");
            assert!(context.contains(&format!("status={status}")));
            assert!(context.contains("providerCode=DeploymentNotFound"));
            assert!(context.contains("providerMessage=Unknown deployment"));
            for hint in ["--endpoint", "ZVEC_GREP_ENDPOINT", "model availability"] {
                assert_eq!(context.contains(hint), status == 404, "{context}");
            }
            assert!(!context.contains("secret"));
            assert!(!error.is_retryable());
            if status == 404 {
                assert!(error.should_fail_fast());
            }
        }
    }
}

#[test]
fn requires_api_key_and_keeps_catalog_endpoint() {
    let error = QwenEmbeddingModel::new(
        config("text", "qwen3.7-text-embedding", 3),
        ModelConfig::default(),
    )
    .err()
    .expect("missing API key");
    assert_eq!(error.code(), crate::EngineError::PERMISSION_DENIED);
}
