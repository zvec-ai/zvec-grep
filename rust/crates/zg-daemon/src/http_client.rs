use std::{borrow::Cow, collections::HashMap, net::IpAddr, sync::Arc};

use bytes::Bytes;
use futures::{StreamExt, stream::BoxStream};
use http::{
    HeaderName, HeaderValue, Method, Request, StatusCode, Uri,
    header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HOST},
};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use rmcp::{
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::streamable_http_client::{
        StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    },
};
use sse_stream::{Sse, SseStream};
use tokio::net::TcpStream;

use crate::DaemonError;

const SESSION_ID: &str = "Mcp-Session-Id";
const LAST_EVENT_ID: &str = "Last-Event-Id";
const PROTOCOL_VERSION: &str = "MCP-Protocol-Version";
const EVENT_STREAM: &str = "text/event-stream";
const JSON: &str = "application/json";

pub(crate) async fn post_json(uri: &str, body: Vec<u8>) -> Result<Vec<u8>, DaemonError> {
    let response = post_response(uri, body).await?;
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|error| DaemonError::McpBridge(error.to_string()))?
        .to_bytes();
    Ok(bytes.to_vec())
}

async fn post_response(uri: &str, body: Vec<u8>) -> Result<http::Response<Incoming>, DaemonError> {
    let mut builder = request_builder(Method::POST, uri)
        .map_err(|error| DaemonError::McpBridge(error.to_string()))?
        .header(CONTENT_TYPE, JSON)
        .header(ACCEPT, JSON);
    if let Some(token) = crate::resolve_token(None)? {
        builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder
        .body(Full::new(Bytes::from(body)))
        .map_err(|error| DaemonError::McpBridge(error.to_string()))?;
    let response = send_http_request(request)
        .await
        .map_err(|error| DaemonError::McpBridge(error.to_string()))?;
    if !response.status().is_success() {
        return Err(DaemonError::McpBridge(
            unexpected_response(response).await.to_string(),
        ));
    }
    Ok(response)
}

pub(crate) async fn post_index_stream(
    uri: &str,
    body: Vec<u8>,
    reporter: &zg_engine::api::index::progress::IndexProgressReporter,
) -> Result<zg_daemon_protocol::ExecutionResult, DaemonError> {
    use zg_daemon_protocol::IndexStreamEvent;
    let mut response = post_response(uri, body).await?.into_body();
    let mut pending = Vec::new();
    while let Some(frame) = response.frame().await {
        let frame = frame.map_err(|error| DaemonError::McpBridge(error.to_string()))?;
        if let Ok(data) = frame.into_data() {
            for byte in data {
                if byte == b'\n' {
                    match serde_json::from_slice::<IndexStreamEvent>(&pending)? {
                        IndexStreamEvent::Progress(progress) => reporter.report(progress),
                        IndexStreamEvent::Finished(result) => return Ok(result),
                    }
                    pending.clear();
                } else {
                    pending.push(byte);
                    if pending.len() > 16 * 1024 * 1024 {
                        return Err(DaemonError::McpBridge(
                            "Index progress event exceeds size limit".into(),
                        ));
                    }
                }
            }
        }
    }
    Err(DaemonError::McpBridge(
        "Index progress stream ended without a result".into(),
    ))
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LoopbackHttpClient;

#[derive(Debug, thiserror::Error)]
pub(crate) enum LoopbackHttpError {
    #[error("HTTP transport failed: {0}")]
    Hyper(#[from] hyper::Error),
    #[error("TCP transport failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid HTTP request: {0}")]
    Http(#[from] http::Error),
    #[error("invalid daemon URL: {0}")]
    InvalidUri(&'static str),
    #[error("MCP JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<LoopbackHttpError> for StreamableHttpError<LoopbackHttpError> {
    fn from(error: LoopbackHttpError) -> Self {
        Self::Client(error)
    }
}

impl StreamableHttpClient for LoopbackHttpClient {
    type Error = LoopbackHttpError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_token,
            custom_headers,
            usize::MAX,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        _max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let body = serde_json::to_vec(&message).map_err(LoopbackHttpError::from)?;
        let mut builder = request_builder(Method::POST, &uri)?
            .header(CONTENT_TYPE, JSON)
            .header(ACCEPT, format!("{EVENT_STREAM}, {JSON}"));
        builder = apply_optional_headers(builder, session_id.as_deref(), auth_token.as_deref());
        builder = apply_custom_headers(builder, custom_headers)?;
        let session_attached = session_id.is_some();
        let response = send_http_request(
            builder
                .body(Full::new(Bytes::from(body)))
                .map_err(LoopbackHttpError::from)?,
        )
        .await?;

        if response.status() == StatusCode::NOT_FOUND && session_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        if matches!(
            response.status(),
            StatusCode::ACCEPTED | StatusCode::NO_CONTENT
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if !response.status().is_success() {
            return Err(unexpected_response(response).await);
        }

        let content_type = response.headers().get(CONTENT_TYPE).cloned();
        let content_length = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let response_session = session_header(&response);
        if content_length == Some(0)
            && matches!(
                message,
                ClientJsonRpcMessage::Notification(_)
                    | ClientJsonRpcMessage::Response(_)
                    | ClientJsonRpcMessage::Error(_)
            )
        {
            return Ok(StreamableHttpPostResponse::Accepted);
        }

        match content_type.as_ref().and_then(header_text) {
            Some(value) if value.starts_with(EVENT_STREAM) => Ok(StreamableHttpPostResponse::Sse(
                sse_stream(response),
                response_session,
            )),
            Some(value) if value.starts_with(JSON) => {
                let bytes = response
                    .into_body()
                    .collect()
                    .await
                    .map_err(LoopbackHttpError::from)?
                    .to_bytes();
                let message = serde_json::from_slice::<ServerJsonRpcMessage>(&bytes)
                    .map_err(LoopbackHttpError::from)?;
                Ok(StreamableHttpPostResponse::Json(message, response_session))
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(
                content_type.and_then(|value| header_text(&value).map(str::to_owned)),
            )),
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let mut builder = request_builder(Method::DELETE, &uri)?;
        builder = apply_optional_headers(builder, Some(session_id.as_ref()), auth_token.as_deref());
        builder = apply_custom_headers(builder, custom_headers)?;
        let response = send_http_request(
            builder
                .body(Full::new(Bytes::new()))
                .map_err(LoopbackHttpError::from)?,
        )
        .await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED || response.status().is_success() {
            return Ok(());
        }
        Err(unexpected_response(response).await)
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, sse_stream::Error>>, StreamableHttpError<Self::Error>>
    {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_token,
            custom_headers,
            usize::MAX,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        _max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, sse_stream::Error>>, StreamableHttpError<Self::Error>>
    {
        let mut builder =
            request_builder(Method::GET, &uri)?.header(ACCEPT, format!("{EVENT_STREAM}, {JSON}"));
        builder = apply_optional_headers(builder, session_id.as_deref(), auth_token.as_deref());
        if let Some(event_id) = last_event_id {
            builder = builder.header(LAST_EVENT_ID, event_id);
        }
        builder = apply_custom_headers(builder, custom_headers)?;
        let response = send_http_request(
            builder
                .body(Full::new(Bytes::new()))
                .map_err(LoopbackHttpError::from)?,
        )
        .await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if !response.status().is_success() {
            return Err(unexpected_response(response).await);
        }
        let content_type = response.headers().get(CONTENT_TYPE).and_then(header_text);
        if !content_type.is_some_and(|value| value.starts_with(EVENT_STREAM)) {
            return Err(StreamableHttpError::UnexpectedContentType(
                content_type.map(str::to_owned),
            ));
        }
        Ok(sse_stream(response))
    }
}

fn request_builder(
    method: Method,
    uri: &str,
) -> Result<http::request::Builder, StreamableHttpError<LoopbackHttpError>> {
    let uri = uri
        .parse::<Uri>()
        .map_err(|_| LoopbackHttpError::InvalidUri("URL cannot be parsed"))?;
    let authority = uri
        .authority()
        .ok_or(LoopbackHttpError::InvalidUri("URL has no authority"))?;
    let host = authority.as_str().to_owned();
    Ok(Request::builder()
        .method(method)
        .uri(uri)
        .header(HOST, host))
}

fn apply_optional_headers(
    mut builder: http::request::Builder,
    session_id: Option<&str>,
    auth_token: Option<&str>,
) -> http::request::Builder {
    if let Some(session_id) = session_id {
        builder = builder.header(SESSION_ID, session_id);
    }
    if let Some(auth_token) = auth_token {
        builder = builder.header(AUTHORIZATION, format!("Bearer {auth_token}"));
    }
    builder
}

fn apply_custom_headers(
    mut builder: http::request::Builder,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> Result<http::request::Builder, StreamableHttpError<LoopbackHttpError>> {
    for (name, value) in custom_headers {
        let reserved = [ACCEPT.as_str(), SESSION_ID, LAST_EVENT_ID];
        if reserved
            .iter()
            .any(|candidate| name.as_str().eq_ignore_ascii_case(candidate))
            && !name.as_str().eq_ignore_ascii_case(PROTOCOL_VERSION)
        {
            return Err(StreamableHttpError::ReservedHeaderConflict(
                name.to_string(),
            ));
        }
        builder = builder.header(name, value);
    }
    Ok(builder)
}

async fn send_http_request(
    request: Request<Full<Bytes>>,
) -> Result<http::Response<Incoming>, StreamableHttpError<LoopbackHttpError>> {
    let uri = request.uri();
    if uri.scheme_str() != Some("http") {
        return Err(LoopbackHttpError::InvalidUri("only HTTP is supported").into());
    }
    let authority = uri
        .authority()
        .ok_or(LoopbackHttpError::InvalidUri("URL has no authority"))?;
    let host = authority.host();
    if !is_loopback_host(host) {
        return Err(LoopbackHttpError::InvalidUri("host is not loopback").into());
    }
    let port = authority.port_u16().unwrap_or(80);
    let stream = TcpStream::connect((host, port))
        .await
        .map_err(LoopbackHttpError::from)?;
    let io = TokioIo::new(stream);
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(LoopbackHttpError::from)?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "daemon HTTP connection closed");
        }
    });
    sender
        .send_request(request)
        .await
        .map_err(LoopbackHttpError::from)
        .map_err(Into::into)
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn session_header(response: &http::Response<Incoming>) -> Option<String> {
    response
        .headers()
        .get(SESSION_ID)
        .and_then(header_text)
        .map(str::to_owned)
}

fn header_text(value: &HeaderValue) -> Option<&str> {
    value.to_str().ok()
}

fn sse_stream(
    response: http::Response<Incoming>,
) -> BoxStream<'static, Result<Sse, sse_stream::Error>> {
    SseStream::from_bytes_stream(response.into_body().into_data_stream()).boxed()
}

async fn unexpected_response(
    response: http::Response<Incoming>,
) -> StreamableHttpError<LoopbackHttpError> {
    let status = response.status();
    let body = response.into_body().collect().await.map_or_else(
        |_| "<failed to read response body>".to_owned(),
        |body| String::from_utf8_lossy(&body.to_bytes()).into_owned(),
    );
    StreamableHttpError::UnexpectedServerResponse(Cow::Owned(format!("HTTP {status}: {body}")))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use http::{HeaderName, HeaderValue, Request};
    use rmcp::transport::streamable_http_client::StreamableHttpError;

    use super::{PROTOCOL_VERSION, apply_custom_headers, is_loopback_host};

    #[tokio::test]
    async fn index_stream_delivers_fragmented_progress_before_the_result() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use zg_daemon_protocol::{DaemonReply, ExecutionResult, IndexStreamEvent};
        use zg_engine::api::index::progress::{
            IndexProgress, IndexProgressPhase, IndexProgressReporter,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(stream.read_u8().await.expect("request header"));
            }
            let mut body = [0; 2];
            stream.read_exact(&mut body).await.expect("request body");
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.expect("response headers");
            let progress = IndexStreamEvent::Progress(IndexProgress {
                phase: IndexProgressPhase::Indexing,
                files_total: Some(9),
                files_indexed: Some(2),
                files_failed: None,
                detail: None,
                embedding: None,
            });
            let line = serde_json::to_string(&progress).expect("progress") + "\n";
            for part in line.as_bytes().chunks(7) {
                stream
                    .write_all(format!("{:x}\r\n", part.len()).as_bytes())
                    .await
                    .expect("chunk length");
                stream.write_all(part).await.expect("chunk");
                stream.write_all(b"\r\n").await.expect("chunk end");
            }
            released.await.expect("progress observed before completion");
            let result = IndexStreamEvent::Finished(ExecutionResult::Success(DaemonReply::Index(
                Box::default(),
            )));
            let line = serde_json::to_string(&result).expect("result") + "\n";
            stream
                .write_all(format!("{:x}\r\n{line}\r\n0\r\n\r\n", line.len()).as_bytes())
                .await
                .expect("result chunk");
        });
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let reporter = IndexProgressReporter::new(move |progress| {
            let _ = sender.send(progress);
        });
        let client = tokio::spawn(async move {
            super::post_index_stream(
                &format!("http://{address}/admin/index"),
                b"{}".to_vec(),
                &reporter,
            )
            .await
        });
        let progress = tokio::time::timeout(std::time::Duration::from_secs(3), receiver.recv())
            .await
            .expect("live progress timeout")
            .expect("progress");
        assert_eq!(progress.files_indexed, Some(2));
        assert!(!client.is_finished());
        release.send(()).expect("release server");
        assert!(matches!(
            client.await.expect("client").expect("result"),
            ExecutionResult::Success(DaemonReply::Index(_))
        ));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn incomplete_index_stream_is_an_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(stream.read_u8().await.expect("header"));
            }
            let mut body = [0; 2];
            stream.read_exact(&mut body).await.expect("body");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{")
                .await
                .expect("truncated event");
        });
        let reporter = zg_engine::api::index::progress::IndexProgressReporter::new(|_| {});
        let error = super::post_index_stream(
            &format!("http://{address}/admin/index"),
            b"{}".to_vec(),
            &reporter,
        )
        .await
        .expect_err("missing final result");
        assert!(error.to_string().contains("without a result"));
        server.await.expect("server");
    }

    #[test]
    fn custom_headers_reject_transport_owned_values() {
        let mut headers = HashMap::new();
        headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("text/plain"),
        );
        assert!(matches!(
            apply_custom_headers(Request::builder(), headers),
            Err(StreamableHttpError::ReservedHeaderConflict(_))
        ));
    }

    #[test]
    fn protocol_version_header_is_forwarded() {
        let mut headers = HashMap::new();
        headers.insert(
            HeaderName::from_bytes(PROTOCOL_VERSION.as_bytes()).expect("valid header"),
            HeaderValue::from_static("2025-11-25"),
        );
        assert!(apply_custom_headers(Request::builder(), headers).is_ok());
    }

    #[test]
    fn http_client_accepts_only_loopback_hosts() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("example.com"));
        assert!(!is_loopback_host("192.0.2.1"));
    }
}
