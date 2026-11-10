use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::mpsc,
};

use super::*;
use crate::models::artifacts::test_support::ipv6_unavailable;

fn assert_user_agent(request: &str) {
    let expected = concat!("zvec-grep/", env!("CARGO_PKG_VERSION"));
    assert!(request.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("user-agent") && value.trim() == expected
        })
    }));
}

fn respond_partial(stream: &mut TcpStream) {
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        BYTES.len()
    );
    stream
        .write_all(headers.as_bytes())
        .expect("write response headers");
    stream.write_all(&BYTES[..4]).expect("write partial body");
    stream.flush().expect("flush partial body");
}

#[tokio::test]
async fn connection_closed_before_response_headers_retries_same_source() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(2, |index, request, stream| {
        assert!(request.contains("/owner/model/resolve/hf-revision/"));
        if index == 0 {
            stream
                .shutdown(std::net::Shutdown::Both)
                .expect("close connection before response");
        } else {
            respond(stream, 200, BYTES);
        }
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let resolved = resolve(root.path(), Some(&server.base_url), &reporter(&events))
        .await
        .expect("same-source request recovery");

    assert_eq!(server.finish(), 2);
    assert!(resolved.paths[ARTIFACTS[0].path].starts_with(root.path().join("huggingface")));
    assert!(partial_files(root.path()).is_empty());
    assert!(!root.path().join("modelscope").exists());
}

#[tokio::test]
async fn interrupted_stream_retries_same_source_and_publishes_only_verified_bytes() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(2, |index, request, mut stream| {
        assert!(request.contains("/owner/model/resolve/hf-revision/"));
        assert_user_agent(&request);
        if index == 0 {
            respond_partial(&mut stream);
        } else {
            respond(stream, 200, BYTES);
        }
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let resolved = resolve(root.path(), Some(&server.base_url), &reporter(&events))
        .await
        .expect("same-source recovery");

    assert_eq!(server.finish(), 2);
    let destination = root.path().join("huggingface").join(ARTIFACTS[0].path);
    assert_eq!(resolved.paths[ARTIFACTS[0].path], destination);
    assert_eq!(async_fs::read(destination).await.expect("artifact"), BYTES);
    assert!(partial_files(root.path()).is_empty());
    assert!(!root.path().join("modelscope").exists());
}

#[tokio::test]
async fn failed_ipv6_response_retries_over_ipv4_before_switching_sources() {
    let root = tempfile::tempdir().expect("cache root");
    let ipv4_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind IPv4 server");
    let port = ipv4_listener.local_addr().expect("IPv4 address").port();
    let ipv6_listener = match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
        Ok(listener) => listener,
        Err(error) if ipv6_unavailable(&error) => {
            // Some CI hosts disable IPv6 entirely; the same-source recovery test
            // above still exercises the retry and publication contract there.
            return;
        }
        Err(error) => panic!("bind IPv6 server: {error}"),
    };
    let ipv6 = TestServer::spawn_on(ipv6_listener, 1, |_index, request, mut stream| {
        assert!(request.contains("/owner/model/resolve/hf-revision/"));
        assert_user_agent(&request);
        respond_partial(&mut stream);
    });
    let ipv4 = TestServer::spawn_on(ipv4_listener, 1, |_index, request, stream| {
        assert!(request.contains("/owner/model/resolve/hf-revision/"));
        assert_user_agent(&request);
        respond(stream, 200, BYTES);
    });
    let addresses = [
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
    ];
    let builder = || {
        reqwest::Client::builder()
            .no_proxy()
            .resolve_to_addrs("artifact.test", &addresses)
    };
    // Model the failing IPv6 path deterministically; Happy Eyeballs must not
    // race it against the healthy IPv4 server during the first attempt.
    let client = ArtifactHttpClient::with_builders(
        builder().local_address(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        builder(),
    );
    let base_url = format!("http://artifact.test:{port}");
    let events = Arc::new(Mutex::new(Vec::new()));
    let resolved = resolve_model_artifacts(
        &client,
        ResolveArtifacts {
            model: "local/test-model",
            sources: sources(root.path(), Some(&base_url)),
            artifacts: ARTIFACTS,
            reporter: &reporter(&events),
            signal: None,
        },
    )
    .await
    .expect("IPv6 failure recovered over IPv4");

    assert_eq!(ipv6.finish(), 1);
    assert_eq!(ipv4.finish(), 1);
    assert!(resolved.paths[ARTIFACTS[0].path].starts_with(root.path().join("huggingface")));
    assert!(partial_files(root.path()).is_empty());
    assert!(!root.path().join("modelscope").exists());
}

#[tokio::test]
async fn network_failures_are_bounded_to_two_attempts_per_source() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(4, |index, request, mut stream| {
        let expected_repo = if index < 2 {
            "/owner/model/"
        } else {
            "/iic/model/"
        };
        assert!(request.contains(expected_repo));
        respond_partial(&mut stream);
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let error = resolve(root.path(), Some(&server.base_url), &reporter(&events))
        .await
        .expect_err("both source attempts failed");

    assert_eq!(server.finish(), 4);
    let message = error.to_string();
    assert!(message.contains("Hugging Face"), "{message}");
    assert!(message.contains("ModelScope"), "{message}");
    assert!(partial_files(root.path()).is_empty());
    for directory in ["huggingface", "modelscope"] {
        assert!(!root.path().join(directory).join(ARTIFACTS[0].path).exists());
    }
}

#[tokio::test]
async fn cancellation_during_ipv4_retry_does_not_switch_sources_or_leave_partial_files() {
    let root = tempfile::tempdir().expect("cache root");
    let signal = CancellationToken::new();
    let (cancelled_tx, cancelled_rx) = mpsc::channel();
    let cancelled_rx = Mutex::new(cancelled_rx);
    let server = TestServer::spawn(2, move |index, request, mut stream| {
        assert!(request.contains("/owner/model/"));
        respond_partial(&mut stream);
        if index == 1 {
            cancelled_rx
                .lock()
                .expect("cancellation receiver")
                .recv_timeout(Duration::from_secs(5))
                .expect("client cancelled after reading retry bytes");
        }
    });
    let observed_requests = Arc::clone(&server.requests);
    let cancel_after_read = signal.clone();
    let reporter = ModelDownloadProgressReporter::new(
        "local/test-model",
        Some(Arc::new(move |event| {
            if observed_requests.load(Ordering::SeqCst) == 2
                && matches!(
                    event,
                    ModelProgress::Downloading {
                        downloaded_bytes: Some(4),
                        ..
                    }
                )
            {
                cancel_after_read.cancel();
                cancelled_tx.send(()).expect("notify retry server");
            }
        })),
        ARTIFACTS.iter().map(|artifact| artifact.path.to_owned()),
    );
    let error = resolve_model_artifacts(
        &ArtifactHttpClient::new(),
        ResolveArtifacts {
            model: "local/test-model",
            sources: sources(root.path(), Some(&server.base_url)),
            artifacts: ARTIFACTS,
            reporter: &reporter,
            signal: Some(&signal),
        },
    )
    .await
    .expect_err("cancelled during retry");

    assert_eq!(error.code(), crate::EngineError::CANCELLED);
    assert_eq!(server.finish(), 2);
    assert!(partial_files(root.path()).is_empty());
    assert!(!root.path().join("modelscope").exists());
}

#[tokio::test]
async fn forbidden_status_switches_sources_without_an_ipv4_retry() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(2, |index, request, stream| {
        assert_user_agent(&request);
        if index == 0 {
            assert!(request.contains("/owner/model/"));
            respond(stream, 403, b"Forbidden");
        } else {
            assert!(request.contains("/iic/model/"));
            respond(stream, 200, BYTES);
        }
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let resolved = resolve(root.path(), Some(&server.base_url), &reporter(&events))
        .await
        .expect("403 source fallback");

    assert_eq!(server.finish(), 2);
    assert!(resolved.paths[ARTIFACTS[0].path].starts_with(root.path().join("modelscope")));
}

#[tokio::test]
async fn modelscope_redirect_preserves_user_agent_at_the_cdn() {
    let root = tempfile::tempdir().expect("cache root");
    let cdn = TestServer::spawn(1, |_index, request, stream| {
        assert!(request.starts_with("GET /model.onnx "));
        assert_user_agent(&request);
        respond(stream, 200, BYTES);
    });
    let location = format!("{}/model.onnx", cdn.base_url);
    let source = TestServer::spawn(2, move |index, request, mut stream| {
        assert_user_agent(&request);
        if index == 0 {
            assert!(request.contains("/owner/model/"));
            respond(stream, 503, b"");
        } else {
            assert!(request.contains("/iic/model/"));
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(response.as_bytes())
                .expect("redirect to another origin");
        }
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let resolved = resolve(root.path(), Some(&source.base_url), &reporter(&events))
        .await
        .expect("ModelScope CDN download");

    assert_eq!(source.finish(), 2);
    assert_eq!(cdn.finish(), 1);
    assert!(resolved.paths[ARTIFACTS[0].path].starts_with(root.path().join("modelscope")));
    assert_eq!(
        async_fs::read(&resolved.paths[ARTIFACTS[0].path])
            .await
            .expect("artifact"),
        BYTES
    );
}
