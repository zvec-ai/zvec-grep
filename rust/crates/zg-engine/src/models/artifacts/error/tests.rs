use std::{
    io::{self, BufRead, BufReader, Write},
    net::TcpListener,
    thread,
    time::Duration,
};

use super::*;

#[test]
fn only_transport_failures_allow_address_family_retry() {
    let reset = io::Error::new(io::ErrorKind::ConnectionReset, "peer reset connection");
    assert!(transient_network_detail(&reset).is_some());
    let wrapped_reset = io::Error::other(reset);
    assert!(transient_network_detail(&wrapped_reset).is_some());

    // A certificate failure must not become retryable merely because its
    // untrusted message mentions a transient error or a network address.
    let certificate = io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid certificate: ConnectionReset for 2001:db8::1",
    );
    assert!(transient_network_detail(&certificate).is_none());
    let invalid_input = io::Error::new(io::ErrorKind::InvalidInput, "invalid request");
    assert!(transient_network_detail(&invalid_input).is_none());
}

async fn connect_proxy_failure(status: Option<u16>) -> reqwest::Error {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test proxy");
    let address = listener.local_addr().expect("proxy address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept CONNECT");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("CONNECT timeout");
        let mut reader = BufReader::new(stream.try_clone().expect("clone proxy stream"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read CONNECT request");
        assert_eq!(line, "CONNECT artifact.test:443 HTTP/1.1\r\n");
        loop {
            line.clear();
            assert_ne!(
                reader.read_line(&mut line).expect("read CONNECT headers"),
                0
            );
            if line == "\r\n" {
                break;
            }
        }
        if let Some(status) = status {
            // A CONNECT client may close as soon as it sees a rejected status.
            // Format in memory so interpolation cannot split the socket writes.
            let response = format!(
                "HTTP/1.1 {status} Rejected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(response.as_bytes())
                .expect("reject CONNECT");
        }
        // None closes the proxy connection before a tunnel response arrives.
    });
    let proxy = reqwest::Proxy::all(format!("http://private-user:private-password@{address}"))
        .expect("test proxy");
    let result = reqwest::Client::builder()
        .no_proxy()
        .proxy(proxy)
        .build()
        .expect("proxy client")
        .get("https://artifact.test/private-path?token=private-token")
        .timeout(Duration::from_secs(5))
        .send()
        .await;
    server.join().expect("proxy server");
    let error = result.expect_err("CONNECT must fail");
    assert!(!error.is_timeout(), "fixture must fail during CONNECT");
    error
}

#[tokio::test]
async fn connect_proxy_eof_allows_ipv4_retry_without_exposing_secrets() {
    let error = connect_proxy_failure(None).await;
    let failure = network_error("unable to request model artifact", error);
    assert!(failure.ipv4_retry_allowed(), "{failure}");
    let message = failure.to_string();
    for secret in [
        "private-user",
        "private-password",
        "private-path",
        "private-token",
    ] {
        assert!(!message.contains(secret), "{message}");
    }
}

#[tokio::test]
async fn connect_proxy_authentication_and_unsuccessful_responses_do_not_retry() {
    for status in [407, 502] {
        let error = connect_proxy_failure(Some(status)).await;
        let mut cause = Some(&error as &(dyn std::error::Error + 'static));
        let tunnel_error = loop {
            let current = cause.expect("CONNECT failure contains a tunnel error");
            if let Some(error) = current.downcast_ref::<ProxyTunnelError>() {
                break error;
            }
            cause = current.source();
        };
        // In particular, a truncated 407 prefix must not pass this test as a
        // generic unsuccessful tunnel just because both errors prohibit retry.
        assert!(
            matches!(
                (status, tunnel_error),
                (407, ProxyTunnelError::ProxyAuthRequired)
                    | (502, ProxyTunnelError::TunnelUnsuccessful)
            ),
            "CONNECT status {status} must retain its structured error"
        );
        let failure = network_error("unable to request model artifact", error);
        assert!(!failure.ipv4_retry_allowed(), "status {status}: {failure}");
    }
}

#[tokio::test]
async fn transport_diagnostics_keep_failure_category_without_request_secrets() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let address = listener.local_addr().expect("server address");
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept request");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("request timeout");
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).expect("read headers"), 0);
            if line == "\r\n" {
                break;
            }
        }
        // Close after reading the request without sending a response.
    });
    let error = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("HTTP client")
        .get(format!(
            "http://private-user:private-password@{address}/private-path?token=private-token"
        ))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect_err("server closed connection");
    server.join().expect("server");
    let failure = network_error("unable to request model artifact", error);
    assert!(failure.ipv4_retry_allowed());
    let message = failure.to_string();
    assert!(message.contains("connection closed before response completed"));
    for secret in [
        "private-user",
        "private-password",
        "private-path",
        "private-token",
    ] {
        assert!(!message.contains(secret), "{message}");
    }
}
