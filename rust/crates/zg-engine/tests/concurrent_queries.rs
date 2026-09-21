use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use zg_engine::{
    EngineError, ZvecGrep,
    api::{
        context::{
            ContextOptions, ContextResult,
            options::{ContextRoute, ContextRouteMode, RefreshPolicy},
        },
        index::{IndexOptions, options::EmbeddingModelSpec},
    },
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Gate {
    entered: Semaphore,
    release: Semaphore,
}
impl Gate {
    fn new() -> Self {
        Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }
    async fn block(&self) {
        self.entered.add_permits(1);
        self.release.acquire().await.expect("release gate").forget();
    }
    async fn wait(&self) {
        // Parallel fixtures contend during native initialization; this is not a latency assertion.
        tokio::time::timeout(Duration::from_secs(30), self.entered.acquire())
            .await
            .expect("operation reaches embedding gate")
            .expect("gate")
            .forget();
    }
}

struct Fixture {
    root: TempDir,
    engine: Arc<ZvecGrep>,
    index_gate: Arc<Gate>,
    query_gate: Arc<Gate>,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().expect("workspace");
        std::fs::write(
            root.path().join("anchor.txt"),
            "orchard anchor documentation",
        )
        .expect("anchor");
        std::fs::write(root.path().join("changing.txt"), "original documentation").expect("source");
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("embedding server");
        let endpoint = format!(
            "http://{}/embeddings",
            listener.local_addr().expect("address")
        );
        let index_gate = Arc::new(Gate::new());
        let query_gate = Arc::new(Gate::new());
        let server = tokio::spawn({
            let index_gate = index_gate.clone();
            let query_gate = query_gate.clone();
            async move {
                let mut handlers = JoinSet::new();
                loop {
                    tokio::select! {
                        connection = listener.accept() => {
                            let (stream, _) = connection.expect("connection");
                            let index_gate = index_gate.clone();
                            let query_gate = query_gate.clone();
                            handlers.spawn(async move { respond(stream, &index_gate, &query_gate).await });
                        }
                        _ = handlers.join_next(), if !handlers.is_empty() => {}
                    }
                }
            }
        });
        let engine = Arc::new(ZvecGrep::new());
        engine.enable_read_session_cache().expect("read cache");
        engine
            .index(IndexOptions {
                root: Some(root.path().to_path_buf()),
                embedding: Some(EmbeddingModelSpec {
                    reference: "qwen/text-embedding-v4".into(),
                    revision: None,
                    cache_dir: None,
                    endpoint: None,
                    device: zg_engine::api::index::options::Device::Cpu,
                }),
                endpoint: Some(endpoint),
                api_key: Some("local-test-key".into()),
                allow_remote: true,
                ..IndexOptions::default()
            })
            .await
            .expect("initial index");
        Self {
            root,
            engine,
            index_gate,
            query_gate,
            server,
        }
    }

    fn query(&self, policy: RefreshPolicy) -> ContextOptions {
        ContextOptions {
            root: Some(self.root.path().to_path_buf()),
            auto_update: false,
            refresh: Some(policy),
            allow_remote: true,
            api_key: Some("local-test-key".into()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: "orchard".into(),
            }],
            ..ContextOptions::default()
        }
    }

    async fn wait_for_query(&self, reading: &mut JoinHandle<Result<ContextResult, EngineError>>) {
        tokio::select! {
            () = self.query_gate.wait() => {},
            result = reading => panic!("query completed before embedding gate: {result:?}"),
        }
    }

    async fn start_update(
        &self,
        rebuild: bool,
    ) -> JoinHandle<Result<zg_engine::api::index::IndexResult, EngineError>> {
        std::fs::write(
            self.root.path().join("changing.txt"),
            "hold-index vineyard documentation",
        )
        .expect("changed source");
        let options = IndexOptions {
            root: Some(self.root.path().to_path_buf()),
            rebuild,
            api_key: Some("local-test-key".into()),
            allow_remote: true,
            ..IndexOptions::default()
        };
        let engine = self.engine.clone();
        let mut update = tokio::spawn(async move { engine.index(options).await });
        tokio::select! {
            () = self.index_gate.wait() => {},
            result = &mut update => panic!("index completed before gate: {result:?}"),
        }
        update
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        self.engine.close();
    }
}

async fn respond(
    mut stream: TcpStream,
    index_gate: &Gate,
    query_gate: &Gate,
) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let (body_start, body_length) = loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .expect("content length")
                .trim()
                .parse::<usize>()
                .expect("length");
            break (end + 4, length);
        }
    };
    while bytes.len() < body_start + body_length {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    let request: Value = serde_json::from_slice(&bytes[body_start..body_start + body_length])?;
    let inputs = request["input"].as_array().expect("text inputs");
    for (text, gate) in [("hold-index", index_gate), ("hold-query", query_gate)] {
        if inputs
            .iter()
            .any(|input| input.as_str().expect("text").contains(text))
        {
            gate.block().await;
        }
    }
    let dimension =
        usize::try_from(request["dimensions"].as_u64().expect("dimensions")).expect("dimension");
    let mut vector = vec![0.0_f32; dimension];
    vector[0] = 1.0;
    let body = serde_json::to_vec(
        &json!({"data": inputs.iter().enumerate().map(|(index, _)| json!({"index": index, "embedding": vector})).collect::<Vec<_>>() }),
    )?;
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&body).await
}

#[tokio::test]
async fn compatible_current_index_queries_borrow_writer_but_wait_does_not() -> TestResult {
    let fixture = Fixture::new().await;
    let update = fixture.start_update(false).await;
    for policy in [RefreshPolicy::Off, RefreshPolicy::Background] {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            fixture.engine.context(fixture.query(policy)),
        )
        .await??;
        assert_eq!(result.items[0].relative_path, PathBuf::from("anchor.txt"));
        assert!(!update.is_finished());
    }
    let mut query = fixture.query(RefreshPolicy::Wait);
    query.lock_timeout_ms = Some(20);
    let error = fixture
        .engine
        .context(query)
        .await
        .expect_err("wait cannot borrow a partial writer");
    assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
    assert!(error.to_string().contains("timed out"));
    fixture.index_gate.release.add_permits(1);
    update.await??;
    let mut query = fixture.query(RefreshPolicy::Wait);
    query.routes[0].query = "vineyard".into();
    let result = fixture.engine.context(query).await?;
    assert_eq!(result.items[0].relative_path, PathBuf::from("changing.txt"));
    Ok(())
}

#[tokio::test]
async fn mismatched_models_and_rebuilds_wait_instead_of_borrowing() -> TestResult {
    let fixture = Fixture::new().await;
    let update = fixture.start_update(false).await;
    for (api_key, endpoint, model_cache) in [
        (Some("different-key".into()), None, None),
        (None, Some("http://127.0.0.1:9/embeddings".into()), None),
        (
            None,
            None,
            Some(fixture.root.path().join("different-cache")),
        ),
    ] {
        let mut query = fixture.query(RefreshPolicy::Off);
        if api_key.is_some() {
            query.api_key = api_key;
        }
        query.endpoint = endpoint;
        query.model_cache = model_cache;
        query.lock_timeout_ms = Some(20);
        let error = fixture
            .engine
            .context(query)
            .await
            .expect_err("model configuration must match");
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        assert!(error.to_string().contains("timed out"));
    }
    fixture.index_gate.release.add_permits(1);
    update.await??;
    let update = fixture.start_update(true).await;
    let mut query = fixture.query(RefreshPolicy::Background);
    query.lock_timeout_ms = Some(20);
    assert_eq!(
        fixture
            .engine
            .context(query)
            .await
            .expect_err("unpublished generation is private")
            .code(),
        EngineError::RESOURCE_BUSY
    );
    fixture.index_gate.release.add_permits(1);
    update.await??;
    Ok(())
}

#[tokio::test]
async fn cancelling_a_waiting_query_releases_its_admission() -> TestResult {
    let fixture = Fixture::new().await;
    let update = fixture.start_update(false).await;
    let signal = CancellationToken::new();
    let mut query = fixture.query(RefreshPolicy::Wait);
    query.signal = Some(signal.clone());
    let waiting = tokio::spawn({
        let engine = fixture.engine.clone();
        async move { engine.context(query).await }
    });
    tokio::task::yield_now().await;
    signal.cancel();
    assert_eq!(
        waiting.await?.expect_err("cancel query").code(),
        EngineError::CANCELLED
    );
    fixture.index_gate.release.add_permits(1);
    update.await??;
    fixture
        .engine
        .context(fixture.query(RefreshPolicy::Off))
        .await?;
    Ok(())
}

#[tokio::test]
async fn writer_finishing_or_aborting_keeps_borrowed_queries_alive() -> TestResult {
    for abort_writer in [false, true] {
        let fixture = Fixture::new().await;
        let update = fixture.start_update(false).await;
        let mut query = fixture.query(RefreshPolicy::Off);
        query.routes = vec![ContextRoute {
            mode: ContextRouteMode::Vector,
            query: "hold-query".into(),
        }];
        let mut reading = tokio::spawn({
            let engine = fixture.engine.clone();
            async move { engine.context(query).await }
        });
        fixture.wait_for_query(&mut reading).await;
        if abort_writer {
            update.abort();
        } else {
            fixture.index_gate.release.add_permits(1);
        }
        // A mismatched query cannot bypass the home lock held by the borrowed reader.
        let mut exclusive = fixture.query(RefreshPolicy::Wait);
        exclusive.lock_timeout_ms = Some(30);
        assert_eq!(
            fixture
                .engine
                .context(exclusive)
                .await
                .expect_err("borrower retains the home lock")
                .code(),
            EngineError::RESOURCE_BUSY
        );
        if !abort_writer {
            assert!(
                !update.is_finished(),
                "writer waits for borrowed queries before closing storage"
            );
        }
        fixture.query_gate.release.add_permits(1);
        assert!(!reading.await??.items.is_empty());
        if abort_writer {
            assert!(update.await.expect_err("writer aborted").is_cancelled());
        } else {
            update.await??;
        }
    }
    Ok(())
}

#[tokio::test]
async fn cancelling_a_borrower_unblocks_writer_retirement() -> TestResult {
    let fixture = Fixture::new().await;
    let update = fixture.start_update(false).await;
    let signal = CancellationToken::new();
    let mut query = fixture.query(RefreshPolicy::Off);
    query.signal = Some(signal.clone());
    query.routes = vec![ContextRoute {
        mode: ContextRouteMode::Vector,
        query: "hold-query".into(),
    }];
    let mut reading = tokio::spawn({
        let engine = fixture.engine.clone();
        async move { engine.context(query).await }
    });
    fixture.wait_for_query(&mut reading).await;
    fixture.index_gate.release.add_permits(1);
    signal.cancel();
    assert_eq!(
        reading.await?.expect_err("borrowed query cancelled").code(),
        EngineError::CANCELLED
    );
    tokio::time::timeout(Duration::from_secs(5), update).await???;
    fixture
        .engine
        .context(fixture.query(RefreshPolicy::Off))
        .await?;
    Ok(())
}

#[tokio::test]
async fn background_update_waits_for_live_query_and_blocks_later_reads() -> TestResult {
    let fixture = Fixture::new().await;
    let mut query = fixture.query(RefreshPolicy::Off);
    query.routes = vec![ContextRoute {
        mode: ContextRouteMode::Vector,
        query: "hold-query".into(),
    }];
    let mut reading = tokio::spawn({
        let engine = fixture.engine.clone();
        async move { engine.context(query).await }
    });
    fixture.wait_for_query(&mut reading).await;
    std::fs::write(
        fixture.root.path().join("changing.txt"),
        "hold-index vineyard documentation",
    )?;
    let update = tokio::spawn({
        let engine = fixture.engine.clone();
        let root = fixture.root.path().to_path_buf();
        async move {
            engine
                .index(IndexOptions {
                    root: Some(root),
                    allow_remote: true,
                    api_key: Some("local-test-key".into()),
                    ..IndexOptions::default()
                })
                .await
        }
    });
    let intent = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.root.path().join(".zvec-grep/locks/write-intent"))?;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if intent.try_lock().is_err() {
                break;
            }
            intent.unlock().expect("release admission probe");
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let mut late = fixture.query(RefreshPolicy::Off);
    late.lock_timeout_ms = Some(20);
    assert_eq!(
        fixture
            .engine
            .context(late)
            .await
            .expect_err("late query waits behind writer")
            .code(),
        EngineError::RESOURCE_BUSY
    );
    assert!(!update.is_finished());
    fixture.query_gate.release.add_permits(1);
    reading.await??;
    fixture.index_gate.wait().await;
    fixture
        .engine
        .context(fixture.query(RefreshPolicy::Background))
        .await?;
    fixture.index_gate.release.add_permits(1);
    update.await??;
    Ok(())
}
