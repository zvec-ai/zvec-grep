use std::{
    io::{self, BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::*;
use crate::domain::model::ModelProgress;
use crate::models::artifacts::ArtifactSourceConfig;

const BYTES: &[u8] = b"verified model artifact";
const ARTIFACTS: &[ArtifactConfig] = &[ArtifactConfig {
    path: "onnx/model q4.onnx",
    size: 23,
    sha256: "7d5fb89e0bde2d0860867ba417c5d6809f6a38ba911715f5e5e3258c9d856813",
}];
const HF: ArtifactSourceConfig = ArtifactSourceConfig {
    repo: "owner/model",
    revision: "hf-revision",
};
const MS: ArtifactSourceConfig = ArtifactSourceConfig {
    repo: "iic/model",
    revision: "ms-revision",
};

fn sources(root: &Path, base_url: Option<&str>) -> [ArtifactSource; 2] {
    let hugging_face = ArtifactSource::hugging_face(HF, root.join("huggingface"));
    let model_scope = ArtifactSource::model_scope(MS, root.join("modelscope"));
    match base_url {
        Some(base_url) => [
            hugging_face.with_base_url(base_url),
            model_scope.with_base_url(base_url),
        ],
        None => [hugging_face, model_scope],
    }
}

fn reporter(events: &Arc<Mutex<Vec<ModelProgress>>>) -> ModelDownloadProgressReporter {
    let captured = Arc::clone(events);
    ModelDownloadProgressReporter::new(
        "local/test-model",
        Some(Arc::new(move |event| {
            captured.lock().expect("progress lock").push(event);
        })),
        ARTIFACTS.iter().map(|artifact| artifact.path.to_owned()),
    )
}

async fn resolve(
    root: &Path,
    base_url: Option<&str>,
    reporter: &ModelDownloadProgressReporter,
) -> Result<ResolvedArtifacts, ModelError> {
    resolve_model_artifacts(
        &reqwest::Client::new(),
        ResolveArtifacts {
            model: "local/test-model",
            sources: sources(root, base_url),
            artifacts: ARTIFACTS,
            reporter,
            signal: None,
        },
    )
    .await
}

struct TestServer {
    base_url: String,
    requests: Arc<AtomicUsize>,
    join: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn spawn(
        expected_requests: usize,
        handler: impl Fn(usize, String, TcpStream) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&requests);
        let handler = Arc::new(handler);
        let join = thread::spawn(move || {
            let mut workers = Vec::new();
            for index in 0..expected_requests {
                let (stream, _) = listener.accept().expect("accept test request");
                counted.fetch_add(1, Ordering::SeqCst);
                let handler = Arc::clone(&handler);
                workers.push(thread::spawn(move || {
                    let mut line = String::new();
                    BufReader::new(stream.try_clone().expect("clone request stream"))
                        .read_line(&mut line)
                        .expect("read request line");
                    handler(index, line, stream);
                }));
            }
            for worker in workers {
                worker.join().expect("request worker");
            }
        });
        Self {
            base_url: format!("http://{address}"),
            requests,
            join: Some(join),
        }
    }

    fn finish(mut self) -> usize {
        self.join
            .take()
            .expect("server join")
            .join()
            .expect("server");
        self.requests.load(Ordering::SeqCst)
    }
}

fn respond(mut stream: TcpStream, status: u16, body: &[u8]) {
    let reason = if status == 200 { "OK" } else { "Unavailable" };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("write response headers");
    stream.write_all(body).expect("write response body");
}

fn partial_files(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .flat_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                partial_files(&path)
            } else if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".part-"))
            {
                vec![path]
            } else {
                Vec::new()
            }
        })
        .collect()
}

#[test]
fn encodes_source_urls_like_node() {
    let source = ArtifactSource::hugging_face(
        ArtifactSourceConfig {
            repo: "owner/model's name",
            revision: "release/(one)!",
        },
        PathBuf::from("cache"),
    );
    assert_eq!(
        artifact_url(&source, "onnx/model q4.onnx"),
        "https://huggingface.co/owner/model's%20name/resolve/release%2F(one)!/onnx/model%20q4.onnx"
    );
}

#[test]
fn fingerprints_manifests_like_node() {
    let source = ArtifactSource::hugging_face(HF, PathBuf::from("cache"));
    let manifest =
        Manifest::new("local/test-model", &source, ARTIFACTS).expect("artifact manifest");

    assert_eq!(manifest.fingerprint, "2b94620c9ab4a8539f10bf74");
}

#[tokio::test]
async fn accepts_node_shaped_completion_marker() {
    let root = tempfile::tempdir().expect("cache root");
    let source = ArtifactSource::hugging_face(HF, root.path().to_owned());
    let destination = source
        .local_path(&ARTIFACTS[0])
        .expect("artifact destination");
    async_fs::create_dir_all(destination.parent().expect("artifact parent"))
        .await
        .expect("create cache");
    async_fs::write(&destination, BYTES)
        .await
        .expect("write artifact");
    let manifest =
        Manifest::new("local/test-model", &source, ARTIFACTS).expect("artifact manifest");
    let metadata = fs::metadata(&destination).expect("artifact metadata");
    let stamp = complete_file_stamp(&metadata);
    let marker = format!(
        r#"{{"version":1,"fingerprint":"{}","files":{{"{}":{{"size":{},"mtimeMs":{},"ctimeMs":{}}}}}}}"#,
        manifest.fingerprint, ARTIFACTS[0].path, stamp.size, stamp.mtime_ms, stamp.ctime_ms,
    );
    async_fs::write(&manifest.marker_path, marker)
        .await
        .expect("write Node marker");

    assert!(
        has_valid_complete_marker(&source, ARTIFACTS, &manifest)
            .await
            .expect("validate marker")
    );
}

#[test]
fn completion_stamps_allow_only_json_round_trip_precision() {
    let timestamp = 1_790_048_381_253.380_1_f64;
    let stamp = CompleteFileStamp {
        size: 23,
        mtime_ms: timestamp,
        ctime_ms: timestamp,
    };
    let adjacent = CompleteFileStamp {
        size: 23,
        mtime_ms: f64::from_bits(timestamp.to_bits() - 1),
        ctime_ms: f64::from_bits(timestamp.to_bits() + 1),
    };
    assert!(complete_file_stamps_match(&stamp, &adjacent));

    let changed = CompleteFileStamp {
        size: 23,
        mtime_ms: f64::from_bits(timestamp.to_bits() - 2),
        ctime_ms: timestamp,
    };
    assert!(!complete_file_stamps_match(&stamp, &changed));
    assert!(!complete_file_stamps_match(
        &stamp,
        &CompleteFileStamp {
            size: 24,
            ..stamp.clone()
        }
    ));
}

#[tokio::test]
async fn checks_modelscope_cache_before_networking_and_repairs_marker() {
    let root = tempfile::tempdir().expect("cache root");
    let destination = root.path().join("modelscope").join(ARTIFACTS[0].path);
    async_fs::create_dir_all(destination.parent().expect("artifact parent"))
        .await
        .expect("create cache");
    async_fs::write(&destination, BYTES)
        .await
        .expect("write cache");
    let events = Arc::new(Mutex::new(Vec::new()));
    let result = resolve(root.path(), None, &reporter(&events))
        .await
        .expect("offline ModelScope cache");
    assert_eq!(result.paths[ARTIFACTS[0].path], destination);
    assert!(
        fs::read_dir(root.path().join("modelscope"))
            .expect("cache entries")
            .any(|entry| entry
                .expect("cache entry")
                .file_name()
                .to_string_lossy()
                .ends_with(".complete"))
    );
}

#[tokio::test]
async fn replaces_same_size_corruption_with_verified_artifact() {
    let root = tempfile::tempdir().expect("cache root");
    let destination = root.path().join("huggingface").join(ARTIFACTS[0].path);
    async_fs::create_dir_all(destination.parent().expect("artifact parent"))
        .await
        .expect("create cache");
    async_fs::write(&destination, vec![b'x'; BYTES.len()])
        .await
        .expect("corrupt cache");
    let server = TestServer::spawn(1, |_index, request, stream| {
        assert!(request.contains("/owner/model/resolve/hf-revision/"));
        respond(stream, 200, BYTES);
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    resolve(root.path(), Some(&server.base_url), &reporter(&events))
        .await
        .expect("repair cache");
    assert_eq!(server.finish(), 1);
    assert_eq!(async_fs::read(&destination).await.expect("artifact"), BYTES);
    assert!(events.lock().expect("events").iter().any(|event| matches!(
        event,
        ModelProgress::Downloading {
            downloaded_bytes: Some(0),
            total_bytes: Some(23),
            ..
        }
    )));
}

#[tokio::test]
async fn falls_back_once_after_integrity_failure_and_preserves_progress_warning() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(2, |_index, request, stream| {
        if request.contains("/owner/model/") {
            respond(stream, 200, &vec![b'x'; BYTES.len()]);
        } else {
            assert!(request.contains("/iic/model/resolve/ms-revision/"));
            respond(stream, 200, BYTES);
        }
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let result = resolve(root.path(), Some(&server.base_url), &reporter(&events))
        .await
        .expect("ModelScope fallback");
    assert_eq!(server.finish(), 2);
    assert!(result.paths[ARTIFACTS[0].path].starts_with(root.path().join("modelscope")));
    assert_eq!(
        events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| matches!(event, ModelProgress::Warning { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn unavailable_hugging_face_falls_back_to_modelscope() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(2, |_index, request, stream| {
        if request.contains("/owner/model/") {
            respond(stream, 503, b"");
        } else {
            assert!(request.contains("/iic/model/resolve/ms-revision/"));
            respond(stream, 200, BYTES);
        }
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let result = resolve(root.path(), Some(&server.base_url), &reporter(&events))
        .await
        .expect("ModelScope fallback");

    assert_eq!(server.finish(), 2);
    assert!(result.paths[ARTIFACTS[0].path].starts_with(root.path().join("modelscope")));
    assert_eq!(
        events
            .lock()
            .expect("events")
            .iter()
            .filter(|event| matches!(event, ModelProgress::Warning { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn interrupted_stream_falls_back_and_removes_partial_file() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(2, |_index, request, mut stream| {
        if request.contains("/owner/model/") {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                BYTES.len()
            )
            .expect("write response headers");
            stream.write_all(&BYTES[..4]).expect("write partial body");
        } else {
            respond(stream, 200, BYTES);
        }
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let resolved = resolve(root.path(), Some(&server.base_url), &reporter(&events))
        .await
        .expect("interrupted stream fallback");
    assert!(resolved.paths[ARTIFACTS[0].path].starts_with(root.path().join("modelscope")));
    assert_eq!(server.finish(), 2);
    assert!(partial_files(root.path()).is_empty());
}

#[tokio::test]
async fn does_not_fallback_for_unauthorized_or_cancelled_requests() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(1, |_index, _request, stream| respond(stream, 401, b""));
    let events = Arc::new(Mutex::new(Vec::new()));
    let error = resolve(root.path(), Some(&server.base_url), &reporter(&events))
        .await
        .expect_err("401 must fail");
    assert!(error.to_string().contains("401"));
    assert_eq!(server.finish(), 1);

    let signal = CancellationToken::new();
    signal.cancel();
    let error = resolve_model_artifacts(
        &reqwest::Client::new(),
        ResolveArtifacts {
            model: "local/test-model",
            sources: sources(root.path(), None),
            artifacts: ARTIFACTS,
            reporter: &reporter(&events),
            signal: Some(&signal),
        },
    )
    .await
    .expect_err("cancelled resolution");
    assert_eq!(error.code(), crate::EngineError::CANCELLED);
}

#[tokio::test]
async fn modelscope_fallback_preserves_cancellation_code() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(1, |_index, request, stream| {
        assert!(request.contains("/owner/model/"));
        respond(stream, 503, b"");
    });
    let signal = CancellationToken::new();
    let cancel_on_fallback = signal.clone();
    let reporter = ModelDownloadProgressReporter::new(
        "local/test-model",
        Some(Arc::new(move |event| {
            if matches!(event, ModelProgress::Warning { .. }) {
                cancel_on_fallback.cancel();
            }
        })),
        ARTIFACTS.iter().map(|artifact| artifact.path.to_owned()),
    );
    let error = resolve_model_artifacts(
        &reqwest::Client::new(),
        ResolveArtifacts {
            model: "local/test-model",
            sources: sources(root.path(), Some(&server.base_url)),
            artifacts: ARTIFACTS,
            reporter: &reporter,
            signal: Some(&signal),
        },
    )
    .await
    .expect_err("ModelScope cancellation must be preserved");

    assert_eq!(error.code(), crate::EngineError::CANCELLED);
    assert_eq!(server.finish(), 1);
}

#[tokio::test]
async fn response_header_timeout_falls_back_without_total_download_deadline() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(2, |_index, request, stream| {
        if request.contains("/owner/model/") {
            thread::sleep(Duration::from_millis(75));
            let _ = stream.shutdown(std::net::Shutdown::Both);
        } else {
            respond(stream, 200, BYTES);
        }
    });
    let short = Duration::from_millis(20);
    let sources = sources(root.path(), Some(&server.base_url))
        .map(|source| source.with_timeouts(short, Duration::from_millis(50)));
    let events = Arc::new(Mutex::new(Vec::new()));
    let resolved = resolve_model_artifacts(
        &reqwest::Client::new(),
        ResolveArtifacts {
            model: "local/test-model",
            sources,
            artifacts: ARTIFACTS,
            reporter: &reporter(&events),
            signal: None,
        },
    )
    .await
    .expect("timeout fallback");
    assert!(resolved.paths[ARTIFACTS[0].path].starts_with(root.path().join("modelscope")));
    assert_eq!(server.finish(), 2);
}

#[tokio::test]
async fn read_idle_timeout_falls_back_and_preserves_both_source_errors() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(2, |_index, request, mut stream| {
        if request.contains("/owner/model/") {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                BYTES.len()
            )
            .expect("write response headers");
            stream.flush().expect("flush response headers");
            thread::sleep(Duration::from_millis(75));
        } else {
            respond(stream, 500, b"");
        }
    });
    let short = Duration::from_millis(20);
    let sources = sources(root.path(), Some(&server.base_url))
        .map(|source| source.with_timeouts(Duration::from_millis(50), short));
    let events = Arc::new(Mutex::new(Vec::new()));
    let error = resolve_model_artifacts(
        &reqwest::Client::new(),
        ResolveArtifacts {
            model: "local/test-model",
            sources,
            artifacts: ARTIFACTS,
            reporter: &reporter(&events),
            signal: None,
        },
    )
    .await
    .expect_err("both sources must fail");
    let message = error.to_string();
    assert!(message.contains("Hugging Face"), "{message}");
    assert!(message.contains("ModelScope"), "{message}");
    assert!(message.contains("timed out"), "{message}");
    assert!(message.contains("500"), "{message}");
    assert_eq!(server.finish(), 2);
}

#[tokio::test]
async fn concurrent_resolvers_share_one_download() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(1, |_index, _request, stream| {
        thread::sleep(Duration::from_millis(40));
        respond(stream, 200, BYTES);
    });
    let first_events = Arc::new(Mutex::new(Vec::new()));
    let second_events = Arc::new(Mutex::new(Vec::new()));
    let first_reporter = reporter(&first_events);
    let second_reporter = reporter(&second_events);
    let (first, second) = tokio::join!(
        resolve(root.path(), Some(&server.base_url), &first_reporter),
        resolve(root.path(), Some(&server.base_url), &second_reporter),
    );
    first.expect("first resolver");
    second.expect("second resolver");
    assert_eq!(server.finish(), 1);
}

#[tokio::test]
async fn completion_marker_does_not_hide_same_size_mutation() {
    let root = tempfile::tempdir().expect("cache root");
    let server = TestServer::spawn(2, |_index, _request, stream| respond(stream, 200, BYTES));
    let events = Arc::new(Mutex::new(Vec::new()));
    let reporter = reporter(&events);
    let first = resolve(root.path(), Some(&server.base_url), &reporter)
        .await
        .expect("first download");
    let path = &first.paths[ARTIFACTS[0].path];
    thread::sleep(Duration::from_millis(5));
    async_fs::write(path, vec![b'x'; BYTES.len()])
        .await
        .expect("mutate cache");
    resolve(root.path(), Some(&server.base_url), &reporter)
        .await
        .expect("repair mutation");
    assert_eq!(server.finish(), 2);
    assert_eq!(async_fs::read(path).await.expect("repaired"), BYTES);
}

#[test]
fn stale_locks_are_recovered_without_allowing_old_owner_cleanup() {
    let root = tempfile::tempdir().expect("lock root");
    let lock_path = root.path().join("artifact.lock");
    let old = CacheLock::try_acquire(&lock_path)
        .expect("acquire old lock")
        .expect("old lock");
    let displaced = root.path().join("displaced.lock");
    fs::rename(&lock_path, &displaced).expect("simulate stale takeover");
    let successor = CacheLock::try_acquire(&lock_path)
        .expect("acquire successor")
        .expect("successor lock");
    drop(old);
    assert!(successor.owner_path.is_file());
    drop(successor);

    fs::create_dir(&lock_path).expect("create stale lock");
    fs::write(lock_path.join(".owner-abandoned"), b"{}").expect("write stale owner");
    let future = SystemTime::now() + LOCK_STALE_AFTER + Duration::from_secs(1);
    assert!(
        remove_stale_lock_at(&lock_path, future, LOCK_STALE_AFTER).expect("recover stale lock")
    );
    assert!(!lock_path.exists());

    fs::create_dir(&lock_path).expect("create dead-owner lock");
    let owner = LockOwner {
        token: "dead".to_owned(),
        pid: i32::MAX as u32,
        hostname: System::host_name().unwrap_or_default(),
    };
    fs::write(
        lock_path.join(".owner-dead"),
        serde_json::to_vec(&owner).expect("serialize dead owner"),
    )
    .expect("write dead owner");
    if !owner.hostname.is_empty() {
        assert!(
            remove_stale_lock_at(&lock_path, SystemTime::now(), LOCK_STALE_AFTER)
                .expect("recover dead owner")
        );
    }
}

#[test]
fn disappearing_lock_components_request_an_acquire_retry() {
    let root = tempfile::tempdir().expect("lock root");
    let lock_path = root.path().join("artifact.lock");
    fs::create_dir(&lock_path).expect("create lock");
    let metadata = fs::metadata(&lock_path).expect("lock metadata");
    fs::remove_dir(&lock_path).expect("release lock");
    assert!(
        inspect_existing_lock(&lock_path, &metadata)
            .expect("disappearing lock is not an error")
            .is_none()
    );

    fs::create_dir(&lock_path).expect("recreate lock");
    let owner_path = lock_path.join(".owner-racing");
    fs::write(&owner_path, b"{}").expect("write owner");
    let owner = fs::read_dir(&lock_path)
        .expect("read lock")
        .next()
        .expect("owner entry")
        .expect("read owner entry");
    fs::remove_file(&owner_path).expect("release owner");
    assert!(
        lock_entry_metadata(&owner)
            .expect("disappearing owner is not an error")
            .is_none()
    );
}

#[test]
fn directory_conflicts_survive_release_without_hiding_permission_errors() {
    let root = tempfile::tempdir().expect("lock root");
    let lock_path = root.path().join("artifact.lock");
    let error = |kind| io::Error::new(kind, "rename fixture");

    // The competing directory may disappear after rename reports the
    // conflict, so these error kinds must not depend on a second lookup.
    assert!(is_directory_conflict(
        &error(io::ErrorKind::AlreadyExists),
        &lock_path
    ));
    assert!(is_directory_conflict(
        &error(io::ErrorKind::DirectoryNotEmpty),
        &lock_path
    ));

    // PermissionDenied is Windows' ambiguous spelling: it is contention
    // only while the target is an actual directory.
    assert!(!is_directory_conflict(
        &error(io::ErrorKind::PermissionDenied),
        &lock_path
    ));
    fs::create_dir(&lock_path).expect("competing lock directory");
    assert!(is_directory_conflict(
        &error(io::ErrorKind::PermissionDenied),
        &lock_path
    ));
    fs::remove_dir(&lock_path).expect("remove competing lock directory");
    fs::write(&lock_path, b"not a lock directory").expect("unrelated file");
    assert!(!is_directory_conflict(
        &error(io::ErrorKind::PermissionDenied),
        &lock_path
    ));
    assert!(!is_directory_conflict(
        &error(io::ErrorKind::Other),
        &lock_path
    ));
}

#[test]
fn initial_owner_disappearance_requests_an_acquire_retry() {
    let root = tempfile::tempdir().expect("lock root");
    let lock_path = root.path().join("artifact.lock");
    let owner_path = lock_path.join(".owner-displaced");
    let lock = CacheLock {
        lock_path,
        owner_path,
        last_heartbeat: std::sync::Mutex::new(UNIX_EPOCH),
    };

    assert!(
        lock.finish_acquire()
            .expect("missing initial owner is not an error")
            .is_none()
    );
}
