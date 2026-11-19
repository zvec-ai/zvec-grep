use std::{
    io::{self, BufRead, BufReader, Write},
    net::{Ipv6Addr, TcpListener},
    thread,
    time::Duration,
};

use super::*;
use crate::models::artifacts::test_support::ipv6_unavailable;

#[tokio::test]
async fn ipv4_client_rejects_ipv6_literal_while_default_client_connects() {
    let listener = match TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) {
        Ok(listener) => listener,
        Err(error) if ipv6_unavailable(&error) => {
            return;
        }
        Err(error) => panic!("bind IPv6 loopback: {error}"),
    };
    let address = listener.local_addr().expect("IPv6 listener address");
    let accepted = listener.try_clone().expect("clone IPv6 listener");
    let server = thread::spawn(move || {
        let (mut stream, _) = accepted.accept().expect("accept default client request");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set request timeout");
        let mut request = BufReader::new(stream.try_clone().expect("clone request stream"));
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            assert_ne!(request.read_line(&mut line).expect("read HTTP request"), 0);
            if line == "\r\n" {
                break;
            }
            headers.push_str(&line);
        }
        assert!(headers.to_ascii_lowercase().contains(&format!(
            "user-agent: {}\r\n",
            USER_AGENT.to_ascii_lowercase()
        )));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .expect("write HTTP response");
    });
    let clients = ArtifactHttpClient::with_builders(
        reqwest::Client::builder().no_proxy(),
        reqwest::Client::builder().no_proxy(),
    );
    let url = format!("http://{address}/model");
    let response = clients
        .default_client()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("default client supports IPv6");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.text().await.expect("HTTP response body"), "ok");
    server.join().expect("IPv6 server");

    // Keep the listener alive so failure cannot be explained by a closed port.
    let error = clients
        .ipv4_client()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect_err("IPv4 fallback must not connect to IPv6");
    assert!(error.is_connect(), "{error:?}");
    listener
        .set_nonblocking(true)
        .expect("set nonblocking listener");
    assert_eq!(
        listener
            .accept()
            .expect_err("no IPv4 client connection")
            .kind(),
        io::ErrorKind::WouldBlock
    );
}
