use std::{
    fs,
    net::TcpListener,
    process::{Command, Output, Stdio},
    sync::Arc,
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
        assert!(stdout.contains("Workspace index: ready"), "{stdout}");
        assert!(
            stdout.contains(&format!("Embedding: {reference}")),
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

struct GatedEmbedding {
    endpoint: String,
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
    server: JoinHandle<()>,
}

impl GatedEmbedding {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fake embedding listener");
        let endpoint = format!(
            "http://{}/embeddings",
            listener.local_addr().expect("address")
        );
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(0));
        let server = tokio::spawn({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            async move {
                let mut handlers = JoinSet::new();
                loop {
                    tokio::select! {
                        connection = listener.accept() => {
                            let (stream, _) = connection.expect("embedding connection");
                            let entered = Arc::clone(&entered);
                            let release = Arc::clone(&release);
                            handlers.spawn(async move {
                                respond_embedding(stream, &entered, &release).await
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
    let request: serde_json::Value =
        serde_json::from_slice(&bytes[body_start..body_start + body_length])?;
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
fn missing_index_uses_default_local_model_without_polluting_query_stdout() {
    let fixture = Fixture::new();
    // Empty workspaces and FTS-only queries never download or execute a model.
    let output = fixture.success(&["--mode", "direct", "--fts", "needle", "--no-color"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No index found; creating one with local/potion-code-16m-v2."),
        "{stderr}"
    );
    assert!(stderr.contains("Index complete"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "No matches.\n");
    fixture.assert_model("direct", "local/potion-code-16m-v2");
}

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
        let output = success(command.output().expect("configured query"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("No index found; creating one with {expected}.")),
            "{stderr}"
        );
        fixture.assert_model("direct", expected);
    }
}

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
        let output = success(command.output().expect("query"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("No index found; creating one with {expected}.")),
            "{stderr}"
        );
        assert!(stderr.contains("Index complete"), "{stderr}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "No matches.\n");
        let status = success(
            fixture
                .command(&["--status", "--mode", mode, "--no-color"])
                .current_dir(workspace.path())
                .output()
                .expect("query workspace status"),
        );
        let stdout = String::from_utf8_lossy(&status.stdout);
        assert!(stdout.contains("Workspace index: ready"), "{stdout}");
        assert!(
            stdout.contains(&format!("Embedding: {expected}")),
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
        assert!(!stderr.contains("Index complete"), "{stderr}");
        let status = fixture.success(&["--status", "--mode", mode, "--no-color"]);
        let stdout = String::from_utf8_lossy(&status.stdout);
        assert!(stdout.contains("Workspace index: disabled"), "{stdout}");
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
            assert!(!stderr.contains("Index complete"), "{stderr}");
            assert_eq!(String::from_utf8_lossy(&output.stdout), "No matches.\n");
            fixture.assert_model(mode, reference);
            assert!(!nested.join(".zvec-grep").exists());
        }
    }
}

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
        let output = success(
            fixture
                .command(&[
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
                ])
                .arg(&cache)
                .env("ZVEC_GREP_ENDPOINT", "https://example.invalid/override")
                .output()
                .expect("query with runtime overrides"),
        );
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
