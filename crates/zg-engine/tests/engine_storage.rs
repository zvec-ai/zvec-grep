use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde_json::{Value, json};
use tempfile::tempdir;
use zg_engine::{
    EngineError, ZvecGrep,
    api::{
        context::{
            ContextOptions,
            options::{ContextRoute, ContextRouteMode},
        },
        index::IndexOptions,
        info::InfoOptions,
    },
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn public_engine_persists_searches_updates_and_drops_real_storage() -> TestResult {
    let temporary = tempfile::Builder::new()
        .prefix("engine storage ")
        .tempdir()?;
    // Windows canonical paths have a verbatim prefix; the native boundary must handle it.
    let canonical_root = fs::canonicalize(temporary.path())?;
    let root = canonical_root.as_path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(
        root.join("auth.rs"),
        "/// Orchard authentication policy.\npub fn authenticate() -> bool { true }\n",
    )?;
    fs::write(
        root.join("billing.md"),
        "# Billing\n\nInvoices and monthly payments.\n",
    )?;
    fs::write(
        root.join("tmp"),
        "A readable extensionless office memorandum.\n",
    )?;
    fs::write(root.join("opaque"), [0_u8, 255, 0, 128])?;
    fs::write(root.join("state.sqlite"), b"SQLite format 3\0")?;
    fs::create_dir(root.join("nested"))?;

    let engine = ZvecGrep::new();
    let initial = engine.index(index_options(root)).await?;
    assert_eq!(initial.generation, 1);
    assert_eq!(initial.files_added, 3, "{initial:?}");
    assert_eq!(initial.files_failed, 0);
    let info = engine.info(info_options(root)).await?;
    assert!(info.indexed);
    assert!(info.index_path.exists());
    assert_eq!(
        info.workspace_index
            .as_ref()
            .and_then(|index| index.generation),
        Some(1)
    );
    assert_eq!(info.status.as_ref().expect("status").files_indexed, 3);
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("auth.rs")]
    );
    assert_eq!(
        fts_paths(&engine, &root.join("nested"), "invoices").await?,
        [PathBuf::from("billing.md")]
    );

    let hybrid = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            query: Some("orchard authentication".to_owned()),
            auto_update: false,
            ..ContextOptions::default()
        })
        .await?;
    assert!(
        hybrid
            .items
            .iter()
            .any(|item| item.relative_path == Path::new("auth.rs"))
    );
    let calls = server.requests.load(Ordering::Acquire);
    let unchanged = engine.index(index_options(root)).await?;
    assert_eq!(unchanged.files_unchanged, 3);
    assert_eq!(unchanged.generation, 2);
    assert_eq!(server.requests.load(Ordering::Acquire), calls);
    engine.close();
    assert_eq!(
        engine
            .info(info_options(root))
            .await
            .expect_err("closed engine")
            .code(),
        EngineError::RESOURCE_CLOSED
    );
    drop(engine);

    let engine = ZvecGrep::new();
    assert_eq!(
        engine
            .info(info_options(root))
            .await?
            .workspace_index
            .expect("index")
            .generation,
        Some(2)
    );
    assert_eq!(
        fts_paths(&engine, root, "orchard").await?,
        [PathBuf::from("auth.rs")]
    );
    let vector = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Vector,
                query: "orchard authentication".to_owned(),
            }],
            auto_update: false,
            ..ContextOptions::default()
        })
        .await?;
    assert!(
        vector
            .items
            .iter()
            .any(|item| item.relative_path == Path::new("auth.rs"))
    );

    fs::write(
        root.join("auth.rs"),
        "/// Vineyard session renewal policy.\npub fn renew_session() -> bool { false }\n",
    )?;
    fs::remove_file(root.join("billing.md"))?;
    fs::write(
        root.join("support.txt"),
        "Customer support handles delivery inquiries.\n",
    )?;
    let updated = engine.index(index_options(root)).await?;
    assert_eq!(
        (
            updated.files_added,
            updated.files_modified,
            updated.files_deleted
        ),
        (1, 1, 1)
    );
    assert_eq!(updated.generation, 3);
    assert!(fts_paths(&engine, root, "orchard").await?.is_empty());
    assert!(fts_paths(&engine, root, "invoices").await?.is_empty());
    assert_eq!(
        fts_paths(&engine, root, "vineyard").await?,
        [PathBuf::from("auth.rs")]
    );
    assert_eq!(
        fts_paths(&engine, root, "delivery").await?,
        [PathBuf::from("support.txt")]
    );

    let rebuilt = engine
        .index(IndexOptions {
            rebuild: true,
            ..index_options(root)
        })
        .await?;
    assert_eq!((rebuilt.files_added, rebuilt.generation), (3, 1));
    let rebuilt_query = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Vector,
                query: "vineyard session renewal".to_owned(),
            }],
            auto_update: false,
            ..ContextOptions::default()
        })
        .await?;
    assert!(
        rebuilt_query
            .items
            .iter()
            .any(|item| item.relative_path == Path::new("auth.rs"))
    );

    assert!(engine.drop_index(info_options(root)).await?);
    assert!(!engine.drop_index(info_options(root)).await?);
    assert!(!info.index_path.exists());
    assert!(!engine.info(info_options(root)).await?.indexed);
    assert!(root.join("auth.rs").is_file());

    configure_remote_model(root, server.address)?;
    engine.index(index_options(root)).await?;
    fs::remove_file(root.join(".zvec-grep/manifest.json"))?;
    assert!(
        engine.drop_index(info_options(root)).await?,
        "orphaned backend can be removed"
    );
    assert!(!info.index_path.exists());
    engine.close();
    Ok(())
}

#[tokio::test]
async fn public_engine_records_failed_files_and_recovers_on_auto_update() -> TestResult {
    let temporary = tempdir()?;
    let root = temporary.path();
    let server = EmbeddingServer::start()?;
    configure_remote_model(root, server.address)?;
    fs::write(root.join("broken.txt"), [255_u8, 254, 255])?;
    let engine = ZvecGrep::new();
    assert!(engine.index(index_options(root)).await.is_err());
    let info = engine.info(info_options(root)).await?;
    assert_eq!(info.status.expect("status").files_failed, 1);

    fs::write(
        root.join("broken.txt"),
        "Recovered readable nebula documentation.\n",
    )?;
    let result = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: "nebula".to_owned(),
            }],
            ..ContextOptions::default()
        })
        .await?;
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].relative_path, Path::new("broken.txt"));
    let status = engine
        .info(info_options(root))
        .await?
        .status
        .expect("status");
    assert_eq!((status.files_failed, status.files_indexed), (0, 1));
    engine.drop_index(info_options(root)).await?;
    engine.close();
    Ok(())
}

fn index_options(root: &Path) -> IndexOptions {
    IndexOptions {
        root: Some(root.to_path_buf()),
        ..IndexOptions::default()
    }
}

fn info_options(root: &Path) -> InfoOptions {
    InfoOptions {
        root: Some(root.to_path_buf()),
        include_status: true,
    }
}

async fn fts_paths(
    engine: &ZvecGrep,
    root: &Path,
    query: &str,
) -> Result<Vec<PathBuf>, EngineError> {
    let result = engine
        .context(ContextOptions {
            root: Some(root.to_path_buf()),
            routes: vec![ContextRoute {
                mode: ContextRouteMode::Fts,
                query: query.to_owned(),
            }],
            auto_update: false,
            ..ContextOptions::default()
        })
        .await?;
    let mut paths = result
        .items
        .into_iter()
        .map(|item| item.relative_path)
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn configure_remote_model(root: &Path, address: SocketAddr) -> std::io::Result<()> {
    let home = root.join(".zvec-grep");
    fs::create_dir_all(&home)?;
    // Seed credentials in a configuration fixture without changing process-wide environment.
    let manifest = json!({
        "manifestVersion": 1, "id": "fixture-workspace", "name": "fixture", "path": home,
        "rootPaths": [{ "absolutePath": root, "recursive": true }],
        "indexPolicy": "enabled", "embedding": { "provider": "qwen", "model": "text-embedding-v4", "dimension": 1024, "metric": "cosine" },
        "indexVersion": null, "createdTime": 1, "updatedTime": 1,
        "embeddingRuntime": { "apiKey": "local-test-key", "endpoint": format!("http://{address}/embeddings") }
    });
    fs::write(home.join("manifest.json"), serde_json::to_vec(&manifest)?)
}

struct EmbeddingServer {
    address: SocketAddr,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl EmbeddingServer {
    fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let worker = thread::spawn({
            let stop = Arc::clone(&stop);
            let requests = Arc::clone(&requests);
            move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    requests.fetch_add(1, Ordering::Release);
                    respond(stream.expect("mock HTTP connection"))
                        .expect("mock embedding response");
                }
            }
        });
        Ok(Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for EmbeddingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                assert!(result.is_ok(), "mock embedding server failed");
            }
        }
    }
}

fn respond(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        request.extend_from_slice(&buffer[..count]);
        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            assert!(headers.contains("authorization: bearer local-test-key"));
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .expect("content length")
                .trim()
                .parse::<usize>()
                .expect("valid content length");
            break (end + 4, length);
        }
    };
    while request.len() < header_end + content_length {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        request.extend_from_slice(&buffer[..count]);
    }
    let body: Value = serde_json::from_slice(&request[header_end..header_end + content_length])?;
    let dimension = usize::try_from(body["dimensions"].as_u64().expect("dimensions"))
        .expect("usize dimensions");
    let data = body["input"]
        .as_array()
        .expect("text inputs")
        .iter()
        .enumerate()
        .map(|(index, text)| {
            let mut vector = vec![0.0_f32; dimension];
            for word in text
                .as_str()
                .expect("text")
                .split(|character: char| !character.is_alphanumeric())
                .filter(|word| !word.is_empty())
            {
                let hash = word.to_lowercase().bytes().fold(0usize, |hash, byte| {
                    hash.wrapping_mul(31).wrapping_add(usize::from(byte))
                });
                vector[hash % dimension] += 1.0;
            }
            json!({ "index": index, "embedding": vector })
        })
        .collect::<Vec<_>>();
    let response = serde_json::to_vec(&json!({ "data": data }))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    )?;
    stream.write_all(&response)
}
