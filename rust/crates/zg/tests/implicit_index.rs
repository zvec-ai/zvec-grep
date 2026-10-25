use std::{
    fs,
    io::Read,
    net::TcpListener,
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::json;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::Child,
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "support/terminal.rs"]
mod terminal;

struct Fixture {
    root: TempDir,
    user: TempDir,
    server_started: bool,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: TempDir::new().expect("empty workspace"),
            user: TempDir::new().expect("isolated user home"),
            server_started: false,
        }
    }

    fn start_server(&mut self) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("available port");
        let address = listener.local_addr().expect("address").to_string();
        drop(listener);
        self.server_started = true;
        self.success(&["--server", "on", "--listen", &address]);
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_zg"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ZVEC_GREP_") {
                command.env_remove(key);
            }
        }
        command
            .current_dir(self.root.path())
            .env("HOME", self.user.path())
            .env("USERPROFILE", self.user.path())
            .env("ZVEC_GREP_HOME", self.user.path().join("runtime"))
            .env("ZVEC_GREP_MODEL_CACHE", self.user.path().join("models"))
            .env(
                "ZVEC_GREP_AUTHORIZATION_KEY_FILE",
                self.user.path().join("key"),
            )
            .env_remove("DASHSCOPE_API_KEY")
            .env_remove("QWEN_API_KEY")
            .args(args);
        command
    }

    fn success(&self, args: &[&str]) -> Output {
        success(self.command(args).output().expect("run CLI"))
    }

    fn assert_model(&self, mode: &str, reference: &str) {
        let output = self.success(&["--status", "--mode", mode, "--no-color"]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("✓ Workspace index is ready"), "{stdout}");
        assert!(
            stdout.contains(&format!("  Embedding   {reference}")),
            "{stdout}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.server_started {
            let _ = self.command(&["--server", "off"]).output();
        }
    }
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn status_uses_grouped_layout_compact_paths_and_explicit_color_policy() {
    let fixture = Fixture::new();
    let root = fs::canonicalize(fixture.root.path()).expect("canonical workspace");
    let run = |color: &str, no_color: bool| {
        let mut command = fixture.command(&["--status", "--mode", "direct", "--color", color]);
        command.env("HOME", &root).env("USERPROFILE", &root);
        if no_color {
            command.env("NO_COLOR", "1");
        } else {
            command.env_remove("NO_COLOR");
        }
        let output = success(command.output().expect("workspace status"));
        String::from_utf8(output.stdout).expect("UTF-8 status")
    };

    let plain = run("never", false);
    assert!(
        plain.starts_with("? Workspace index is not configured\n  ~\n"),
        "{plain}"
    );
    let storage = std::path::Path::new(".zvec-grep").join("storage");
    assert!(
        plain.contains(&format!("\n\n  Storage     {}", storage.display())),
        "{plain}"
    );
    assert!(!plain.contains("\u{1b}["), "{plain}");
    assert!(!plain.contains("Root:"), "{plain}");
    assert!(!plain.contains("Index path:"), "{plain}");
    assert_eq!(
        run("auto", false),
        plain,
        "piped auto output stays uncolored"
    );
    assert_eq!(
        run("auto", true),
        plain,
        "NO_COLOR suppresses automatic color"
    );
    assert_eq!(run("never", true), plain);

    let colored = run("always", true);
    assert!(
        colored.contains("\u{1b}[33m? Workspace index is not configured\u{1b}[0m"),
        "{colored:?}"
    );
    assert!(colored.contains("\u{1b}[36m~\u{1b}[0m"), "{colored:?}");
    assert!(
        colored.contains("\u{1b}[2mStorage     \u{1b}[0m"),
        "{colored:?}"
    );
    assert!(
        !fixture.root.path().join(".zvec-grep").exists(),
        "status must not create an index"
    );
}

#[test]
fn status_check_ready_keeps_direct_and_server_exit_semantics() {
    let mut fixture = Fixture::new();
    fixture.start_server();
    let check = |heading: &str, ready: bool| {
        for mode in ["direct", "server"] {
            let output = fixture
                .command(&["--status", "--mode", mode, "--check-ready", "--no-color"])
                .output()
                .expect("readiness status");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.success(), ready, "{mode}: {stdout}\n{stderr}");
            assert!(stdout.starts_with(heading), "{mode}: {stdout}");
            assert_eq!(
                stderr.contains("workspace index is not ready"),
                !ready,
                "{mode}: {stderr}"
            );
            if ready {
                assert!(stdout.contains("  Coverage    "), "{mode}: {stdout}");
                assert!(stdout.contains("0 / 0 files"), "{mode}: {stdout}");
            }
        }
    };

    check("? Workspace index is not configured", false);
    // An empty local index never loads or downloads the embedding model.
    fixture.success(&[
        "--index",
        "--mode",
        "direct",
        "--embedding",
        "local/potion-code-16m-v2",
        "--no-color",
    ]);
    check("✓ Workspace index is ready", true);
    fs::write(fixture.root.path().join("added.rs"), "fn added() {}\n").expect("new source file");
    check("! Workspace index needs an update", false);
}

fn with_closed_stdout(mut command: Command) -> Output {
    let (reader, writer) = std::io::pipe().expect("stdout pipe");
    drop(reader);
    command
        .stdout(Stdio::from(writer))
        .output()
        .expect("run with closed stdout")
}

#[test]
fn closed_stdout_preserves_success_and_readiness_errors() {
    let mut fixture = Fixture::new();
    fixture.start_server();
    for args in [
        vec!["--version"],
        vec!["--help"],
        vec!["--server", "status"],
        vec!["--status", "--mode", "direct"],
        vec!["--status", "--mode", "server"],
    ] {
        let output = with_closed_stdout(fixture.command(&args));
        assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
    }
    for mode in ["direct", "server"] {
        let output =
            with_closed_stdout(fixture.command(&["--status", "--mode", mode, "--check-ready"]));
        assert_eq!(output.status.code(), Some(1), "{mode}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "Error: workspace index is not ready\n",
            "{mode}: {output:?}"
        );
    }
    fixture.success(&[
        "--index",
        "--mode",
        "direct",
        "--embedding",
        "local/potion-code-16m-v2",
    ]);
    for mode in ["direct", "server"] {
        for args in [
            vec!["--status", "--mode", mode, "--check-ready"],
            vec!["--fts", "needle", "--mode", mode, "--refresh", "off"],
        ] {
            let output = with_closed_stdout(fixture.command(&args));
            assert_eq!(output.status.code(), Some(0), "{args:?}: {output:?}");
            assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
        }
    }
    fixture.success(&["--server", "off"]);
    fixture.server_started = false;
    let output = with_closed_stdout(fixture.command(&["--server", "status", "--check-ready"]));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "Error: server is not ready\n"
    );
}

#[test]
fn partial_stdout_consumption_keeps_query_successful() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.path().join("source.txt"),
        format!("needle {}\n", "x".repeat(400)).repeat(10_000),
    )
    .expect("large search fixture");
    let mut child = fixture
        .command(&["--rg", "needle", "source.txt", "--mode", "direct"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("query producer");
    let mut stdout = child.stdout.take().expect("query stdout");
    let mut prefix = [0; 100];
    stdout.read_exact(&mut prefix).expect("consume first chunk");
    assert!(String::from_utf8_lossy(&prefix).contains("source.txt"));
    drop(stdout);
    let output = child.wait_with_output().expect("query completion");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

struct GatedEmbedding {
    endpoint: String,
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
    authorized_requests: Arc<AtomicUsize>,
    server: JoinHandle<()>,
}

impl GatedEmbedding {
    async fn start() -> Self {
        Self::start_with_credential(None).await
    }

    async fn start_with_credential(expected_key: Option<String>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fake embedding listener");
        let endpoint = format!(
            "http://{}/embeddings",
            listener.local_addr().expect("address")
        );
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let authorized_requests = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            let authorized_requests = Arc::clone(&authorized_requests);
            async move {
                let mut handlers = JoinSet::new();
                loop {
                    tokio::select! {
                        connection = listener.accept() => {
                            let (stream, _) = connection.expect("embedding connection");
                            let entered = Arc::clone(&entered);
                            let release = Arc::clone(&release);
                            let expected_key = expected_key.clone();
                            let authorized_requests = Arc::clone(&authorized_requests);
                            handlers.spawn(async move {
                                respond_embedding(stream, &entered, &release, expected_key.as_deref(), &authorized_requests).await
                            });
                        }
                        result = handlers.join_next(), if !handlers.is_empty() => {
                            result.expect("embedding handler").expect("embedding task")
                                .expect("embedding response");
                        }
                    }
                }
            }
        });
        Self {
            endpoint,
            entered,
            release,
            authorized_requests,
            server,
        }
    }
}

impl Drop for GatedEmbedding {
    fn drop(&mut self) {
        self.release.close();
        self.server.abort();
    }
}

struct ReleaseIndexGate(Arc<Semaphore>);

impl Drop for ReleaseIndexGate {
    fn drop(&mut self) {
        // Closing also lets any queued background refresh finish during cleanup.
        self.0.close();
    }
}

async fn respond_embedding(
    mut stream: TcpStream,
    entered: &Semaphore,
    release: &Semaphore,
    expected_key: Option<&str>,
    authorized_requests: &AtomicUsize,
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
            if let Some(expected_key) = expected_key
                && !headers.lines().any(|line| {
                    line == format!("authorization: bearer {expected_key}").to_ascii_lowercase()
                })
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Expected the newly configured provider credential",
                ));
            }
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
    let request: serde_json::Value =
        serde_json::from_slice(&bytes[body_start..body_start + body_length])?;
    authorized_requests.fetch_add(1, Ordering::SeqCst);
    let inputs = request["input"].as_array().expect("text inputs");
    if inputs
        .iter()
        .any(|input| input.as_str().expect("text").contains("hold-index"))
    {
        entered.add_permits(1);
        if let Ok(permit) = release.acquire().await {
            permit.forget();
        }
    }
    let dimension =
        usize::try_from(request["dimensions"].as_u64().expect("dimensions")).expect("dimension");
    let mut vector = vec![0.0_f32; dimension];
    vector[0] = 1.0;
    let body = serde_json::to_vec(&json!({
        "data": inputs.iter().enumerate()
            .map(|(index, _)| json!({"index": index, "embedding": vector}))
            .collect::<Vec<_>>()
    }))?;
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&body).await
}

fn spawn_cli(command: Command) -> Result<Child, String> {
    tokio::process::Command::from(command)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("spawn CLI: {error}"))
}

async fn wait_cli(child: &mut Child, limit: Duration) -> Result<Output, String> {
    let mut stdout = child.stdout.take().expect("CLI stdout");
    let mut stderr = child.stderr.take().expect("CLI stderr");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let result = tokio::time::timeout(limit, async {
        tokio::try_join!(
            child.wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
        )
    })
    .await;
    let failure = match result {
        Ok(Ok((status, _, _))) if status.success() => {
            return Ok(Output {
                status,
                stdout: out,
                stderr: err,
            });
        }
        Ok(Ok((status, _, _))) => format!("CLI exited with {status}"),
        result => {
            // Kill and reap before returning a timeout or I/O failure.
            let cleanup = child.kill().await;
            format!("CLI did not finish within {limit:?}: {result:?}; kill/reap: {cleanup:?}")
        }
    };
    Err(format!(
        "{failure}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_cli_queries_do_not_wait_for_incremental_writer() {
    let mut embedding = GatedEmbedding::start().await;
    let mut fixture = Fixture::new();
    // Drop the release guard before Fixture stops the daemon, even on panic.
    let release = ReleaseIndexGate(Arc::clone(&embedding.release));
    fs::write(
        fixture.root.path().join("anchor.txt"),
        "orchard anchor documentation",
    )
    .expect("anchor");
    fs::write(
        fixture.root.path().join("changing.txt"),
        "original documentation",
    )
    .expect("changing source");
    let index_args = |mode| {
        [
            "--index",
            "--mode",
            mode,
            "--embedding",
            "qwen/text-embedding-v4",
            "--endpoint",
            embedding.endpoint.as_str(),
            "--allow-remote",
            "--api-key",
            "local-test-key",
            "--no-color",
        ]
    };
    // Build before starting the daemon so no watcher can race the explicit update.
    let mut initial = spawn_cli(fixture.command(&index_args("direct"))).expect("initial CLI");
    wait_cli(&mut initial, Duration::from_secs(30))
        .await
        .expect("initial index");
    fixture.start_server();
    fs::write(
        fixture.root.path().join("changing.txt"),
        "hold-index vineyard documentation",
    )
    .expect("changed source");
    let mut writer = spawn_cli(fixture.command(&index_args("server"))).expect("incremental CLI");
    let result: Result<(), String> = async {
        tokio::time::timeout(Duration::from_secs(30), async {
            tokio::select! {
                permit = embedding.entered.acquire() => {
                    permit.map_err(|error| error.to_string())?.forget();
                    Ok(())
                }
                status = writer.wait() => Err(format!("index exited before embedding gate: {status:?}")),
            }
        }).await.map_err(|error| format!("index did not reach embedding gate: {error}"))??;
        for mode in ["server", "auto"] {
            for refresh in ["off", "background"] {
                let mut query = spawn_cli(fixture.command(&[
                    "--mode", mode, "--fts", "orchard", "--refresh", refresh,
                    "--allow-remote", "--api-key", "local-test-key", "--no-color",
                ]))?;
                // The unreleased gate proves nonblocking behavior without timing process startup.
                let output = wait_cli(&mut query, Duration::from_secs(30)).await;
                if writer.try_wait().map_err(|error| error.to_string())?.is_some() {
                    return Err("incremental writer exited while its embedding was gated".into());
                }
                let output = output.map_err(|error| format!(
                    "--mode {mode} --refresh {refresh} blocked while incremental embedding remained gated: {error}"
                ))?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !stdout.contains("anchor.txt") {
                    return Err(format!("--mode {mode} --refresh {refresh} did not return anchor.txt: {stdout}"));
                }
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.contains("status: possibly_stale\nresults: served_from_current_index\nbackground_refresh: running") {
                    return Err(format!("--mode {mode} --refresh {refresh} omitted the running index status: {stderr}"));
                }
            }
        }
        Ok(())
    }.await;

    // Finish cleanup before surfacing the regression assertion.
    drop(release);
    let indexed = wait_cli(&mut writer, Duration::from_secs(30)).await;
    let stopped = fixture.command(&["--server", "off"]).output();
    if stopped.as_ref().is_ok_and(|output| output.status.success()) {
        fixture.server_started = false;
    }
    drop(fixture);
    embedding.server.abort();
    let _ = (&mut embedding.server).await;
    indexed.expect("incremental index completes after gate release");
    success(stopped.expect("stop daemon"));
    result.expect("indexed CLI queries must finish before the writer is released");
}

#[test]
fn direct_search_warns_about_unmatched_index_drift_on_stderr() {
    let fixture = Fixture::new();
    fixture.success(&[
        "--index",
        "--mode",
        "direct",
        "--embedding",
        "local/potion-code-16m-v2",
        "--no-color",
    ]);
    let clean = fixture.success(&["--mode", "direct", "--fts", "needle", "--no-color"]);
    assert!(!String::from_utf8_lossy(&clean.stderr).contains("status:"));
    fs::write(
        fixture.root.path().join("unmatched.txt"),
        "unrelated contents",
    )
    .expect("unindexed source");
    let stale = fixture.success(&[
        "--mode",
        "direct",
        "--fts",
        "needle",
        "--refresh",
        "off",
        "--no-color",
    ]);
    assert_eq!(String::from_utf8_lossy(&stale.stdout), "No matches.\n");
    assert!(String::from_utf8_lossy(&stale.stderr).contains("status: possibly_stale\nresults: served_from_current_index\nbackground_refresh: idle (0/1)\n"), "{}", String::from_utf8_lossy(&stale.stderr));
}

#[test]
fn noninteractive_missing_index_requires_explicit_creation() {
    let fixture = Fixture::new();
    // Missing workspaces never download or execute a model without confirmation.
    let output = fixture
        .command(&["--mode", "direct", "--fts", "needle", "--no-color"])
        .output()
        .expect("noninteractive query");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No index found"), "{stderr}");
    assert!(
        stderr.contains("zg --index --embedding <model>"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty());
    assert!(!fixture.root.path().join(".zvec-grep").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn configured_local_models_follow_environment_then_global_default_precedence() {
    for (configured, environment, expected) in [
        (
            "local/potion-retrieval-32m",
            None,
            "local/potion-retrieval-32m",
        ),
        (
            "qwen/text-embedding-v4",
            Some(" local/potion-retrieval-32m "),
            "local/potion-retrieval-32m",
        ),
        (
            "local/potion-retrieval-32m",
            Some("local/potion-multilingual-128m"),
            "local/potion-multilingual-128m",
        ),
        (
            "local/potion-retrieval-32m",
            Some("  "),
            "local/potion-retrieval-32m",
        ),
        ("qwen/text-embedding-v4", None, "local/potion-code-16m-v2"),
        (
            "local/potion-retrieval-32m",
            Some("qwen/text-embedding-v4"),
            "local/potion-code-16m-v2",
        ),
    ] {
        let fixture = Fixture::new();
        fixture.success(&["--config", "model", "set", configured, "--default"]);
        let mut command = fixture.command(&["--mode", "direct", "--fts", "needle", "--no-color"]);
        if let Some(reference) = environment {
            command.env("ZVEC_GREP_EMBEDDING", reference);
        }
        let output = success(terminal::run(command, &[("Enter Build", b"\r")]));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Select an embedding model to build zg index:"),
            "{stderr}"
        );
        fixture.assert_model("direct", expected);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn auto_and_server_build_at_the_query_root_not_the_daemon_working_directory() {
    for (mode, server, expected) in [
        ("auto", false, "local/potion-code-16m-v2"),
        ("server", true, "local/potion-retrieval-32m"),
        ("auto", true, "local/potion-code-16m-v2"),
    ] {
        let mut fixture = Fixture::new();
        if server {
            fixture.start_server();
        }
        let workspace = TempDir::new().expect("query workspace distinct from daemon cwd");
        let mut command = fixture.command(&["--mode", mode, "--fts", "needle", "--no-color"]);
        command.current_dir(workspace.path());
        if mode == "server" {
            // Query-process defaults must take precedence over the daemon's environment.
            command.env("ZVEC_GREP_EMBEDDING", expected);
        }
        let output = success(terminal::run(command, &[("Enter Build", b"\r")]));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Select an embedding model to build zg index:"),
            "{stderr}"
        );
        assert!(stderr.contains("Index ready"), "{stderr}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "No matches.\n");
        let status = success(
            fixture
                .command(&["--status", "--mode", mode, "--no-color"])
                .current_dir(workspace.path())
                .output()
                .expect("query workspace status"),
        );
        let stdout = String::from_utf8_lossy(&status.stdout);
        assert!(stdout.contains("✓ Workspace index is ready"), "{stdout}");
        assert!(
            stdout.contains(&format!("  Embedding   {expected}")),
            "{stdout}"
        );
        assert!(!fixture.root.path().join(".zvec-grep").exists());
    }
}

#[test]
fn disabled_workspace_is_not_rebuilt_from_a_nested_query_directory() {
    let mut fixture = Fixture::new();
    let root = fs::canonicalize(fixture.root.path()).expect("canonical workspace");
    let home = root.join(".zvec-grep");
    fs::create_dir(&home).expect("workspace metadata directory");
    fs::write(
        home.join("manifest.json"),
        serde_json::to_vec(&json!({
            "name": "disabled-workspace",
            "path": home,
            "root": root,
            "indexPolicy": "disabled",
            "indexVersion": null,
            "createdTime": 0,
            "updatedTime": 0
        }))
        .expect("disabled metadata"),
    )
    .expect("save disabled workspace");
    let nested = root.join("nested");
    fs::create_dir(&nested).expect("nested query directory");
    fixture.start_server();
    for mode in ["direct", "server", "auto"] {
        let output = fixture
            .command(&["--mode", mode, "--fts", "needle", "--no-color"])
            .current_dir(&nested)
            .env("ZVEC_GREP_EMBEDDING", "unsupported/model")
            .output()
            .expect("disabled query");
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("No index found"), "{stderr}");
        assert!(!stderr.contains("Index ready"), "{stderr}");
        let status = fixture.success(&["--status", "--mode", mode, "--no-color"]);
        let stdout = String::from_utf8_lossy(&status.stdout);
        assert!(
            stdout.contains("○ Workspace indexing is disabled"),
            "{stdout}"
        );
        assert!(!nested.join(".zvec-grep").exists());
    }
}

#[test]
fn rg_never_inspects_or_builds_an_index_even_in_server_mode() {
    let fixture = Fixture::new();
    for mode in ["direct", "auto", "server"] {
        // Invalid model defaults and an absent daemon must not affect managed rg.
        let output = success(
            fixture
                .command(&["--mode", mode, "--no-color", "--rg", "needle", "."])
                .env("ZVEC_GREP_EMBEDDING", "unsupported/model")
                .output()
                .expect("rg query"),
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "No matches.\n");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("No index found"));
        assert!(!fixture.root.path().join(".zvec-grep").exists());
    }
}

#[test]
fn existing_local_and_remote_indexes_are_reused_from_nested_directories() {
    for reference in ["local/potion-retrieval-32m", "qwen/text-embedding-v4"] {
        let mut fixture = Fixture::new();
        fixture.success(&[
            "--index",
            "--mode",
            "direct",
            "--embedding",
            reference,
            "--allow-remote",
            "--api-key",
            "test-key",
            "--no-color",
        ]);
        let nested = fixture.root.path().join("nested");
        fs::create_dir(&nested).expect("nested directory");
        fixture.start_server();
        for mode in ["direct", "server", "auto"] {
            let output = success(
                fixture
                    .command(&[
                        "--mode",
                        mode,
                        "--fts",
                        "needle",
                        "--refresh",
                        "off",
                        "--no-color",
                    ])
                    .current_dir(&nested)
                    .env("ZVEC_GREP_EMBEDDING", "unsupported/model")
                    .output()
                    .expect("query existing index"),
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!stderr.contains("No index found"), "{stderr}");
            assert!(!stderr.contains("Index ready"), "{stderr}");
            assert_eq!(String::from_utf8_lossy(&output.stdout), "No matches.\n");
            fixture.assert_model(mode, reference);
            assert!(!nested.join(".zvec-grep").exists());
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn implicit_build_preserves_local_overrides_without_persisting_remote_credentials() {
    for mode in ["direct", "server"] {
        let mut fixture = Fixture::new();
        fixture.success(&[
            "--config",
            "model",
            "set",
            "qwen/text-embedding-v4",
            "--default",
            "--endpoint",
            "https://example.invalid/embeddings",
        ]);
        if mode == "server" {
            fixture.start_server();
        }
        let cache = fixture.user.path().join("explicit-model-cache");
        let mut command = fixture.command(&[
            "--mode",
            mode,
            "--fts",
            "needle",
            "--device",
            "cpu",
            "--api-key",
            "implicit-query-secret",
            "--allow-remote",
            "--no-color",
            "--model-cache",
        ]);
        command
            .arg(&cache)
            .env("ZVEC_GREP_ENDPOINT", "https://example.invalid/override");
        let output = success(terminal::run(
            command,
            &[("Enter Continue", b"\x1b[D"), ("Enter Build", b"\r")],
        ));
        fixture.assert_model(mode, "local/potion-code-16m-v2");
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(fixture.root.path().join(".zvec-grep/manifest.json")).expect("manifest"),
        )
        .expect("manifest JSON");
        // These persisted runtime settings are shared with the Node client.
        let runtime = &manifest["embeddingRuntimes"]["local/potion-code-16m-v2"];
        assert_eq!(runtime["device"], "cpu");
        assert_eq!(runtime["cacheDir"], json!(cache));
        assert!(runtime.get("apiKey").is_none());
        assert!(runtime.get("endpoint").is_none());
        assert!(!manifest.to_string().contains("implicit-query-secret"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("implicit-query-secret"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("implicit-query-secret"));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn cancelling_model_selection_exits_without_error_or_index() {
    for (mode, server) in [
        ("direct", false),
        ("server", true),
        ("auto", false),
        ("auto", true),
    ] {
        for cancellation in [b"\x1b".as_slice(), b"\x03".as_slice()] {
            let mut fixture = Fixture::new();
            if server {
                fixture.start_server();
            }
            let output = terminal::run(
                fixture.command(&["--mode", mode, "--fts", "needle", "--no-color"]),
                &[("Esc Exit", cancellation)],
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(130), "{mode}: {stderr}");
            assert!(output.stdout.is_empty());
            assert!(
                !stderr.contains("Index creation cancelled"),
                "{mode}: {stderr}"
            );
            assert!(!stderr.contains("error["), "{mode}: {stderr}");
            assert!(
                !stderr.to_ascii_lowercase().contains("error:"),
                "{mode}: {stderr}"
            );
            assert!(
                stderr.contains("Select an embedding model to build zg index:"),
                "{mode}: {stderr}"
            );
            assert!(!fixture.root.path().join(".zvec-grep").exists());
            assert!(!fixture.user.path().join("models").exists());
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn remote_authorization_can_return_to_model_selection_without_sending_data() {
    let fixture = Fixture::new();
    fixture.success(&[
        "--config",
        "model",
        "set",
        "qwen/text-embedding-v4",
        "--default",
        "--endpoint",
        "http://127.0.0.1:1/no-requests",
    ]);
    let output = success(terminal::run(
        fixture.command(&[
            "--mode",
            "direct",
            "--fts",
            "needle",
            "--device",
            "cpu",
            "--api-key",
            "setup-test-secret",
            "--no-color",
        ]),
        &[
            ("Enter Continue", b"\r"),
            ("Back to model selection", b"\x1b[B\x1b[B\r"),
            ("Enter Continue", b"\x1b[D"),
            ("Enter Build", b"\r"),
        ],
    ));
    fixture.assert_model("direct", "local/potion-code-16m-v2");
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.root.path().join(".zvec-grep/manifest.json")).expect("manifest"),
    )
    .expect("manifest JSON");
    assert_eq!(
        manifest["embeddingRuntimes"]["local/potion-code-16m-v2"]["device"],
        "cpu"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("setup-test-secret"));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "No matches.\n");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn remote_allow_once_covers_the_cli_build_and_following_query_without_a_grant() {
    for mode in ["direct", "server"] {
        let mut fixture = Fixture::new();
        fixture.success(&[
            "--config",
            "model",
            "set",
            "qwen/text-embedding-v4",
            "--default",
            "--endpoint",
            "http://127.0.0.1:1/no-requests",
        ]);
        if mode == "server" {
            fixture.start_server();
        }
        let output = success(terminal::run(
            fixture.command(&[
                "--mode",
                mode,
                "--fts",
                "needle",
                "--api-key",
                "setup-test-secret",
                "--no-color",
            ]),
            &[("Enter Continue", b"\r"), ("Enter Confirm", b"\r")],
        ));
        fixture.assert_model(mode, "qwen/text-embedding-v4");
        assert!(
            !fixture
                .root
                .path()
                .join(".zvec-grep/authorization.json")
                .exists()
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "No matches.\n");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("setup-test-secret"));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_remote_credential_can_be_saved_before_build_and_query() {
    let embedding = GatedEmbedding::start_with_credential(Some("entered-test-secret".into())).await;
    for mode in ["direct", "server"] {
        let mut fixture = Fixture::new();
        fixture.success(&[
            "--config",
            "model",
            "set",
            "qwen/text-embedding-v4",
            "--default",
            "--endpoint",
            &embedding.endpoint,
        ]);
        fs::write(
            fixture.root.path().join("needle.txt"),
            "needle workspace documentation",
        )
        .expect("source file");
        if mode == "server" {
            fixture.start_server();
        }
        let command = fixture.command(&[
            "--mode",
            mode,
            "needle",
            "--device",
            "cpu",
            "--refresh",
            "off",
            "--no-color",
        ]);
        let before = embedding.authorized_requests.load(Ordering::SeqCst);
        let output = success(
            tokio::task::spawn_blocking(move || {
                terminal::run(
                    command,
                    &[
                        ("Enter Build", b"\x1b[C"),
                        ("Enter Continue", b"\r"),
                        ("Enter Save and continue", b"\r"),
                        (
                            "Enter an API Key to continue.",
                            b"\x1b[200~  entered-test-secret\r\n\x1b[201~",
                        ),
                        ("API Key  [ *******************", b"\r"),
                        ("Enter Confirm", b"\x1b[B\x1b[B\r"),
                        ("Credential available", b"\r"),
                        ("Enter Confirm", b"\r"),
                    ],
                )
            })
            .await
            .expect("interactive setup"),
        );
        assert!(
            embedding.authorized_requests.load(Ordering::SeqCst) >= before + 2,
            "the build and subsequent query must both use the entered credential"
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("needle.txt"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("entered-test-secret"));
        assert!(stderr.contains("Index ready"));
        let config: serde_json::Value = serde_json::from_slice(
            &fs::read(fixture.user.path().join(".zvec-grep/config.json")).expect("provider config"),
        )
        .expect("config JSON");
        assert_eq!(config["providers"]["qwen"]["apiKey"], "entered-test-secret");
        assert_eq!(
            config["models"]["qwen/text-embedding-v4"]["endpoint"],
            embedding.endpoint
        );
        let manifest = fs::read_to_string(fixture.root.path().join(".zvec-grep/manifest.json"))
            .expect("index manifest");
        assert!(!manifest.contains("entered-test-secret"));
        assert!(
            !fixture
                .root
                .path()
                .join(".zvec-grep/authorization.json")
                .exists()
        );
        fixture.assert_model(mode, "qwen/text-embedding-v4");
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn cancelling_missing_credential_input_does_not_save_or_build() {
    for mode in ["direct", "server"] {
        for cancellation in [b"\x1b".as_slice(), b"\x03".as_slice()] {
            let mut fixture = Fixture::new();
            if mode == "server" {
                fixture.start_server();
            }
            let output = terminal::run(
                fixture.command(&["--mode", mode, "--fts", "needle", "--no-color"]),
                &[
                    ("Enter Build", b"\x1b[C"),
                    ("Enter Continue", b"\r"),
                    ("Enter Save and continue", b"unsaved-test-secret"),
                    ("API Key  [ *******************", cancellation),
                ],
            );
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert!(!String::from_utf8_lossy(&output.stderr).contains("unsaved-test-secret"));
            assert!(!fixture.user.path().join(".zvec-grep/config.json").exists());
            assert!(!fixture.root.path().join(".zvec-grep").exists());
            assert!(!fixture.user.path().join("models").exists());
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn escaping_remote_authorization_leaves_no_index_or_grant() {
    let fixture = Fixture::new();
    fixture.success(&[
        "--config",
        "model",
        "set",
        "qwen/text-embedding-v4",
        "--default",
        "--endpoint",
        "http://127.0.0.1:1/no-requests",
    ]);
    let output = terminal::run(
        fixture.command(&[
            "--mode",
            "direct",
            "--fts",
            "needle",
            "--api-key",
            "setup-test-secret",
            "--no-color",
        ]),
        &[("Enter Continue", b"\r"), ("Enter Confirm", b"\x1b")],
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!fixture.root.path().join(".zvec-grep").exists());
    assert!(!fixture.user.path().join("models").exists());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn matching_workspace_consent_skips_the_remote_authorization_screen() {
    let fixture = Fixture::new();
    fixture.success(&[
        "--config",
        "model",
        "set",
        "qwen/text-embedding-v4",
        "--default",
        "--endpoint",
        "http://127.0.0.1:1/no-requests",
    ]);
    fixture.success(&[
        "--auth",
        "grant",
        fixture.root.path().to_str().expect("root"),
        "--capability",
        "embedding",
        "--scope",
        "workspace",
        "--embedding",
        "qwen/text-embedding-v4",
    ]);
    let output = success(terminal::run(
        fixture.command(&[
            "--mode",
            "direct",
            "--fts",
            "needle",
            "--api-key",
            "setup-test-secret",
            "--no-color",
        ]),
        &[("Enter Continue", b"\r")],
    ));
    fixture.assert_model("direct", "qwen/text-embedding-v4");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Remote embedding authorization"));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "No matches.\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_index_path_lookups_return_current_text_or_no_matches_without_vector_neighbors() {
    let embedding = GatedEmbedding::start().await;
    let fixture = Fixture::new();
    fs::write(
        fixture.root.path().join("anchor.txt"),
        "Unrelated orchard documentation.\n",
    )
    .expect("source");
    let mut index = spawn_cli(fixture.command(&[
        "--index",
        "--mode",
        "direct",
        "--embedding",
        "qwen/text-embedding-v4",
        "--endpoint",
        &embedding.endpoint,
        "--allow-remote",
        "--api-key",
        "local-test-key",
    ]))
    .expect("index process");
    wait_cli(&mut index, Duration::from_secs(30))
        .await
        .expect("warm index");
    let mut vector = spawn_cli(fixture.command(&[
        "--vector",
        "missing.ts",
        "--mode",
        "direct",
        "--refresh",
        "off",
        "--allow-remote",
        "--api-key",
        "local-test-key",
    ]))
    .expect("explicit vector query");
    let neighbors = wait_cli(&mut vector, Duration::from_secs(30))
        .await
        .expect("vector neighbors");
    assert!(String::from_utf8_lossy(&neighbors.stdout).contains("anchor.txt"));
    // New source references must be visible even though the index has not been refreshed.
    fs::write(
        fixture.root.path().join("references.txt"),
        "Import src/auth.ts here.\n",
    )
    .expect("current reference");
    embedding.server.abort();
    for mode in ["direct", "server", "auto"] {
        for query in ["missing.ts", "src/auth.ts"] {
            let mut command = fixture.command(&[query, "--mode", mode]);
            command.env("ZVEC_GREP_EMBEDDING", "unsupported/model");
            let mut query_process = spawn_cli(command).expect("path query");
            let output = wait_cli(&mut query_process, Duration::from_secs(30))
                .await
                .expect("local lookup");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!stdout.contains("anchor.txt"), "{mode}: {stdout}");
            assert_eq!(
                stderr,
                concat!(
                    "warning: automatically performing path lookup; use --hybrid to force hybrid search.\n",
                    "warning: no matching file paths; searching current file contents literally (rg).\n"
                )
            );
            if query == "missing.ts" {
                assert_eq!(stdout, "No matches.\n");
            } else {
                assert!(stdout.contains("references.txt"), "{stdout}");
                assert!(stdout.contains("Import src/auth.ts"), "{stdout}");
            }
        }
    }
}
