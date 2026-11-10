use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::atomic::AtomicBool,
};

use super::*;
use crate::models::artifacts::test_support::ipv6_unavailable;

type ProxyRequests = Arc<Mutex<Vec<(&'static str, String)>>>;

struct ConnectProxy {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl ConnectProxy {
    fn spawn_on(
        listener: TcpListener,
        route: &'static str,
        primary_status: Option<u16>,
        requests: &ProxyRequests,
    ) -> Self {
        let address = listener.local_addr().expect("proxy address");
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let requests = Arc::clone(requests);
        let join = thread::spawn(move || {
            loop {
                let (stream, _) = listener.accept().expect("accept proxy request");
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("proxy request timeout");
                let mut reader = BufReader::new(stream);
                let mut request = String::new();
                reader
                    .read_line(&mut request)
                    .expect("read CONNECT request");
                let request = request.trim_end().to_owned();
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).expect("read proxy headers"), 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                let status = if request == "CONNECT artifact.test:443 HTTP/1.1" {
                    primary_status
                } else {
                    assert_eq!(request, "CONNECT mirror.test:443 HTTP/1.1");
                    Some(407)
                };
                requests
                    .lock()
                    .expect("proxy requests")
                    .push((route, request));
                if let Some(status) = status {
                    respond_to_connect(reader.into_inner(), status)
                        .expect("write CONNECT response");
                }
                // No response closes CONNECT before a tunnel is established.
            }
        });
        Self {
            address,
            stop,
            join: Some(join),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn finish(mut self) {
        self.shutdown().expect("proxy server");
    }

    fn shutdown(&mut self) -> thread::Result<()> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };
        self.stop.store(true, Ordering::SeqCst);
        // Wake accept after resolution, including when a missing retry means
        // this proxy never received a request. Request counts never control exit.
        let _ = TcpStream::connect(self.address);
        join.join()
    }
}

impl Drop for ConnectProxy {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn respond_to_connect(mut stream: TcpStream, status: u16) -> io::Result<()> {
    // CONNECT clients can reject the status prefix without reading the remaining
    // headers. Formatting directly into TcpStream splits the response into writes,
    // so the client can close before the status or later headers have been sent.
    let response =
        format!("HTTP/1.1 {status} Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(response.as_bytes())
}

fn proxy_builder(url: &str) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .proxy(reqwest::Proxy::all(url).expect("test proxy"))
        .timeout(Duration::from_secs(5))
}

async fn resolve_proxy_failures(client: &ArtifactHttpClient) -> ModelError {
    let root = tempfile::tempdir().expect("cache root");
    let events = Arc::new(Mutex::new(Vec::new()));
    let error = resolve_model_artifacts(
        client,
        ResolveArtifacts {
            model: "local/test-model",
            sources: vec![
                ArtifactSource::hugging_face(HF, root.path().join("huggingface"))
                    .with_base_url("https://artifact.test"),
                ArtifactSource::model_scope(MS, root.path().join("modelscope"))
                    .with_base_url("https://mirror.test"),
            ],
            artifacts: ARTIFACTS,
            reporter: &reporter(&events),
            signal: None,
        },
    )
    .await
    .expect_err("both CONNECT targets fail");
    assert!(partial_files(root.path()).is_empty());
    for directory in ["huggingface", "modelscope"] {
        assert!(!root.path().join(directory).join(ARTIFACTS[0].path).exists());
    }
    error
}

fn assert_retry_order(requests: &ProxyRequests, default_route: &'static str) {
    assert_eq!(
        *requests.lock().expect("proxy requests"),
        [
            (
                default_route,
                "CONNECT artifact.test:443 HTTP/1.1".to_owned()
            ),
            ("ipv4", "CONNECT artifact.test:443 HTTP/1.1".to_owned()),
            (default_route, "CONNECT mirror.test:443 HTTP/1.1".to_owned()),
        ]
    );
}

#[tokio::test]
async fn connect_proxy_eof_retries_ipv4_once_before_switching_sources() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let default = ConnectProxy::spawn_on(
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind default proxy"),
        "default",
        None,
        &requests,
    );
    let ipv4 = ConnectProxy::spawn_on(
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind IPv4 proxy"),
        "ipv4",
        None,
        &requests,
    );
    let client = ArtifactHttpClient::with_builders(
        proxy_builder(&default.url()),
        proxy_builder(&ipv4.url()),
    );

    let error = resolve_proxy_failures(&client).await;
    default.finish();
    ipv4.finish();

    assert_retry_order(&requests, "default");
    assert!(error.to_string().contains("IPv4 retry failed"));
}

#[tokio::test]
async fn connect_proxy_authentication_failure_never_uses_ipv4_retry() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let default = ConnectProxy::spawn_on(
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind default proxy"),
        "default",
        Some(407),
        &requests,
    );
    let ipv4 = ConnectProxy::spawn_on(
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind IPv4 proxy"),
        "ipv4",
        Some(407),
        &requests,
    );
    let client = ArtifactHttpClient::with_builders(
        proxy_builder(&default.url()),
        proxy_builder(&ipv4.url()),
    );

    resolve_proxy_failures(&client).await;
    default.finish();
    ipv4.finish();

    assert_eq!(
        *requests.lock().expect("proxy requests"),
        [
            ("default", "CONNECT artifact.test:443 HTTP/1.1".to_owned()),
            ("default", "CONNECT mirror.test:443 HTTP/1.1".to_owned()),
        ]
    );
}

#[tokio::test]
async fn connect_proxy_eof_over_ipv6_retries_the_same_proxy_over_ipv4() {
    let ipv4_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind IPv4 proxy");
    let port = ipv4_listener
        .local_addr()
        .expect("IPv4 proxy address")
        .port();
    let ipv6_listener = match TcpListener::bind((Ipv6Addr::LOCALHOST, port)) {
        Ok(listener) => listener,
        Err(error) if ipv6_unavailable(&error) => return,
        Err(error) => panic!("bind IPv6 proxy: {error}"),
    };
    let requests = Arc::new(Mutex::new(Vec::new()));
    let ipv6 = ConnectProxy::spawn_on(ipv6_listener, "ipv6", None, &requests);
    let ipv4 = ConnectProxy::spawn_on(ipv4_listener, "ipv4", None, &requests);
    let addresses = [
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
    ];
    let proxy_url = format!("http://proxy.test:{port}");
    let builder = || proxy_builder(&proxy_url).resolve_to_addrs("proxy.test", &addresses);
    let client = ArtifactHttpClient::with_builders(
        builder().local_address(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        builder(),
    );

    resolve_proxy_failures(&client).await;
    ipv6.finish();
    ipv4.finish();

    assert_retry_order(&requests, "ipv6");
}
