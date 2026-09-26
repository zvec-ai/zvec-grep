use std::{
    error::Error,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use serde_json::json;
use tempfile::{NamedTempFile, TempDir};

const SERVER_START_ATTEMPTS: usize = 5;

struct ServerGuard {
    binary: PathBuf,
    home: PathBuf,
    listen: String,
    token_file: Option<PathBuf>,
    active: bool,
}

impl ServerGuard {
    fn stop(&mut self) -> Result<Output, std::io::Error> {
        let mut command = Command::new(&self.binary);
        command.args(["--server", "off", "--home"]).arg(&self.home);
        if let Some(token_file) = &self.token_file {
            command.arg("--token-file").arg(token_file);
        }
        let output = command.output()?;
        if output.status.success() {
            self.active = false;
        }
        Ok(output)
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = self.stop();
        }
    }
}

// Each caller supplies its own TempDir, so cleanup can only stop that test's
// daemon. Keep the guard alive even when bootstrap fails before reporting ready.
fn start_on_available_port<T>(
    binary: &Path,
    home: &TempDir,
    token_file: Option<&Path>,
    mut start: impl FnMut(&str) -> Result<T, Box<dyn Error>>,
) -> Result<(ServerGuard, T), Box<dyn Error>> {
    for attempt in 1..=SERVER_START_ATTEMPTS {
        let mut guard = ServerGuard {
            binary: binary.to_owned(),
            home: home.path().to_owned(),
            listen: format!("127.0.0.1:{}", available_port()?),
            token_file: token_file.map(Path::to_owned),
            active: true,
        };
        let log_path = home.path().join("daemon").join("bootstrap.log");
        let log_start = std::fs::metadata(&log_path).map_or(0, |metadata| metadata.len());
        match start(&guard.listen) {
            Ok(value) => return Ok((guard, value)),
            Err(error) => {
                let log = log_tail(&log_path, log_start);
                let details = format!("{error}\ndaemon log ({}):\n{log}", log_path.display());
                let already_ready = std::fs::read(home.path().join("daemon/instance.lock"))
                    .is_ok_and(|bytes| {
                        serde_json::from_slice::<serde_json::Value>(&bytes)
                            .is_ok_and(|record| record["ready"] == true)
                    });
                let address_in_use = error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::AddrInUse)
                    || is_address_in_use(&details);
                if attempt == SERVER_START_ATTEMPTS || already_ready || !address_in_use {
                    return Err(details.into());
                }
                // Only bind conflicts are retryable. A failed stop must never
                // allow a later attempt to accidentally reuse a surviving daemon.
                assert_command_success(&guard.stop()?);
            }
        }
    }
    unreachable!("server startup returns on the final attempt")
}

fn start_server(
    binary: &Path,
    home: &TempDir,
    toolset: &str,
    token_file: Option<&Path>,
    configure: impl Fn(&mut Command),
) -> Result<(ServerGuard, Output), Box<dyn Error>> {
    start_on_available_port(binary, home, token_file, |listen| {
        let mut command = Command::new(binary);
        command
            .args(["--server", "on", "--home"])
            .arg(home.path())
            .args(["--listen", listen, "--mcp-toolset", toolset]);
        if let Some(token_file) = token_file {
            command.arg("--token-file").arg(token_file);
        }
        configure(&mut command);
        server_start_output(&mut command)
    })
}

fn server_start_output(command: &mut Command) -> Result<Output, Box<dyn Error>> {
    let output = command.output()?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!(
            "server startup failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}

fn is_address_in_use(details: &str) -> bool {
    details.contains("is already in use")
        || details.contains("Address already in use")
        || details.contains("AddrInUse")
        || details.contains("Only one usage of each socket address")
}

fn log_tail(path: &Path, start: u64) -> String {
    use std::io::{Seek, SeekFrom};

    let read = || -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let tail_start = file.metadata()?.len().saturating_sub(8192).max(start);
        file.seek(SeekFrom::Start(tail_start))?;
        let mut bytes = Vec::new();
        file.take(8192).read_to_end(&mut bytes)?;
        let mut lines = String::from_utf8_lossy(&bytes)
            .lines()
            .rev()
            .take(20)
            .map(|line| {
                let lower = line.to_ascii_lowercase();
                if [
                    "api_key",
                    "apikey",
                    "api key",
                    "authorization",
                    "bearer",
                    "token",
                    "credential",
                ]
                .iter()
                .any(|sensitive| lower.contains(sensitive))
                {
                    "[redacted sensitive log line]".to_owned()
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>();
        lines.reverse();
        Ok(lines.join("\n"))
    };
    read().unwrap_or_else(|error| format!("<unavailable: {error}>"))
}

struct StdioBridge {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    reader: Option<JoinHandle<()>>,
    stderr: NamedTempFile,
    home: PathBuf,
    daemon_log_start: u64,
}

impl StdioBridge {
    fn spawn(binary: &Path, home: &Path, listen: &str) -> Result<Self, Box<dyn Error>> {
        Self::spawn_with_toolset(binary, home, listen, Some("full"))
    }

    fn spawn_with_toolset(
        binary: &Path,
        home: &Path,
        listen: &str,
        toolset: Option<&str>,
    ) -> Result<Self, Box<dyn Error>> {
        let stderr = NamedTempFile::new()?;
        let daemon_log_start =
            std::fs::metadata(home.join("daemon").join("logs").join("server.log"))
                .map_or(0, |metadata| metadata.len());
        let mut command = Command::new(binary);
        command.env_remove("ZVEC_GREP_MCP_TOOLSET");
        command.args([
            "--server",
            "--stdio",
            "--home",
            path_text(home)?,
            "--listen",
            listen,
        ]);
        if let Some(toolset) = toolset {
            command.args(["--mcp-toolset", toolset]);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr.reopen()?)
            .spawn()?;
        let stdin = child.stdin.take().ok_or("stdio bridge has no stdin")?;
        let stdout = child.stdout.take().ok_or("stdio bridge has no stdout")?;
        let (sender, lines) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            lines,
            reader: Some(reader),
            stderr,
            home: home.to_owned(),
            daemon_log_start,
        })
    }

    fn request(
        &mut self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, Box<dyn Error>> {
        self.wait_for_response(request, None)
            .map(|(response, _)| response)
    }

    fn notify(&mut self, notification: &serde_json::Value) -> Result<(), Box<dyn Error>> {
        let stdin = self.stdin.as_mut().ok_or("stdio bridge is closed")?;
        writeln!(stdin, "{notification}")?;
        stdin.flush()?;
        Ok(())
    }

    fn request_with_consent(
        &mut self,
        request: &serde_json::Value,
        choice: &str,
    ) -> Result<(serde_json::Value, usize), Box<dyn Error>> {
        self.wait_for_response(request, Some(choice))
    }

    fn wait_for_response(
        &mut self,
        request: &serde_json::Value,
        choice: Option<&str>,
    ) -> Result<(serde_json::Value, usize), Box<dyn Error>> {
        let id = request.get("id").ok_or("request has no id")?;
        let started = Instant::now();
        self.notify(request).map_err(|error| {
            self.response_error(request, choice, 0, started, "none", &error.to_string())
        })?;
        // A blocking index request includes cold tokenizer/storage setup. Use
        // the same 30-second budget as the HTTP indexing fixture, without
        // resetting the deadline when consent or progress notifications arrive.
        let timeout = if request["params"]["name"] == "zvec_grep_index" {
            Duration::from_secs(30)
        } else {
            Duration::from_secs(20)
        };
        let deadline = started + timeout;
        let mut prompts = 0;
        let mut last_message = "none".to_owned();
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|error| {
                    self.response_error(
                        request,
                        choice,
                        prompts,
                        started,
                        &last_message,
                        &error.to_string(),
                    )
                })?;
            let response: serde_json::Value = serde_json::from_str(&line).map_err(|error| {
                self.response_error(
                    request,
                    choice,
                    prompts,
                    started,
                    &last_message,
                    &format!("invalid JSON: {error}"),
                )
            })?;
            // Record protocol metadata only: request arguments and response
            // bodies may contain API keys or indexed source content.
            last_message = json!({
                "id": response.get("id"),
                "method": response.get("method"),
                "error_code": response.pointer("/error/code"),
                "has_result": response.get("result").is_some()
            })
            .to_string();
            if response["method"] == "elicitation/create" {
                prompts += 1;
                let choice = choice.ok_or("unexpected consent prompt")?;
                self.notify(&json!({"jsonrpc": "2.0", "id": response["id"], "result": {
                    "action": "accept", "content": {"choice": choice}
                }}))?;
            } else if response.get("id") == Some(id) {
                return Ok((response, prompts));
            }
        }
    }

    fn response_error(
        &mut self,
        request: &serde_json::Value,
        choice: Option<&str>,
        prompts: usize,
        started: Instant,
        last_message: &str,
        reason: &str,
    ) -> Box<dyn Error> {
        let child_status = match self.child.try_wait() {
            Ok(Some(status)) => status.to_string(),
            Ok(None) => "running".to_owned(),
            Err(error) => format!("unavailable: {error}"),
        };
        let log_path = self.home.join("daemon").join("logs").join("server.log");
        format!(
            "stdio response failed: {reason}; id={}, method={}, tool={}, choice={choice:?}, prompts={prompts}, elapsed={:?}, child={child_status}, last_message={last_message}\nbridge stderr:\n{}\ndaemon log ({}):\n{}",
            request["id"], request["method"], request["params"]["name"], started.elapsed(),
            log_tail(self.stderr.path(), 0), log_path.display(), log_tail(&log_path, self.daemon_log_start)
        ).into()
    }

    fn close(mut self) -> Result<(), Box<dyn Error>> {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait()? {
                if let Some(reader) = self.reader.take() {
                    let _ = reader.join();
                }
                if status.success() {
                    return Ok(());
                }
                return Err(format!("stdio bridge exited with {status}").into());
            }
            if Instant::now() >= deadline {
                self.child.kill()?;
                let _ = self.child.wait();
                return Err("stdio bridge did not exit after stdin closed".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for StdioBridge {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
fn server_start_retries_a_bind_race_without_stopping_the_port_owner() -> Result<(), Box<dyn Error>>
{
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let mut attempts = 0;
    let mut occupied = None;
    let (mut guard, _) = start_on_available_port(&binary, &home, None, |listen| {
        attempts += 1;
        if occupied.is_none() {
            // Claim the port after selection, immediately before daemon startup.
            occupied = Some(TcpListener::bind(listen)?);
        }
        server_start_output(
            Command::new(&binary)
                .args(["--server", "on", "--home"])
                .arg(home.path())
                .args(["--listen", listen, "--mcp-toolset", "full"]),
        )
    })?;
    assert!(attempts >= 2, "the forced bind conflict must be retried");
    let occupied = occupied.ok_or("bind-race listener was not created")?;
    assert_ne!(guard.listen, occupied.local_addr()?.to_string());
    assert_command_success(&guard.stop()?);
    // Retry cleanup is scoped to the private home, never to the occupied port.
    assert!(TcpStream::connect(occupied.local_addr()?).is_ok());
    Ok(())
}

#[test]
fn server_start_does_not_retry_unrelated_failures() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let mut attempts = 0;
    let result = start_on_available_port::<()>(&binary, &home, None, |_| {
        attempts += 1;
        Err("fixture setup failed".into())
    });
    assert_eq!(attempts, 1);
    let error = result.err().ok_or("unexpected startup success")?;
    assert!(error.to_string().contains("fixture setup failed"));
    Ok(())
}

#[test]
fn server_run_failure_is_written_to_bootstrap_and_rotating_logs() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let listen = listener.local_addr()?.to_string();
    let daemon_dir = home.path().join("daemon");
    std::fs::create_dir_all(&daemon_dir)?;
    let bootstrap_path = daemon_dir.join("bootstrap.log");
    let output = Command::new(&binary)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .args(["--server", "run", "--home"])
        .arg(home.path())
        .args(["--listen", &listen])
        .stderr(Stdio::from(std::fs::File::create(&bootstrap_path)?))
        .output()?;
    assert!(!output.status.success());

    let bootstrap = std::fs::read_to_string(&bootstrap_path)?;
    assert!(bootstrap.contains("Error:"), "{bootstrap}");
    let log = std::fs::read_to_string(daemon_dir.join("logs").join("server.log"))?;
    assert!(
        log.lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|record| record["fields"]["message"] == "daemon failed")
        }),
        "{log}"
    );
    Ok(())
}

#[test]
fn duplicate_server_run_preserves_active_daemon_logs() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let config_home = TempDir::new()?;
    let config_dir = config_home.path().join(".zvec-grep");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::write(
        config_dir.join("config.json"),
        r#"{"version":1,"log":{"maxBytes":1,"keep":1}}"#,
    )?;

    let (mut guard, _) = start_server(&binary, &home, "agent", None, |command| {
        command
            .env("HOME", config_home.path())
            .env("USERPROFILE", config_home.path());
    })?;
    let log_dir = home.path().join("daemon").join("logs");
    let active_path = log_dir.join("server.log");
    let backup_path = log_dir.join("server.log.1");
    let active_before = std::fs::read(&active_path)?;
    let backup_before = std::fs::read(&backup_path)?;
    assert!(!backup_before.is_empty());

    let duplicate = Command::new(&binary)
        .env("HOME", config_home.path())
        .env("USERPROFILE", config_home.path())
        .args(["--server", "run", "--home"])
        .arg(home.path())
        .args(["--listen", &guard.listen])
        .output()?;
    assert!(!duplicate.status.success());
    assert!(
        String::from_utf8_lossy(&duplicate.stderr).contains("already running"),
        "{}",
        String::from_utf8_lossy(&duplicate.stderr)
    );
    assert_eq!(std::fs::read(&active_path)?, active_before);
    assert_eq!(std::fs::read(&backup_path)?, backup_before);

    let status = Command::new(&binary)
        .args(["--server", "status", "--home"])
        .arg(home.path())
        .arg("--check-ready")
        .output()?;
    assert_command_success(&status);
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn server_on_exposes_only_agent_search_and_off_stops_it() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let (mut guard, output) = start_server(&binary, &home, "agent", None, |_| {})?;
    let port = guard.listen.parse::<SocketAddr>()?.port();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Server: ready"));
    assert!(stdout.contains("MCP toolset: agent"));

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "zg-test", "version": "1" }
        }
    });
    let response = post_json(port, None, &initialize.to_string())?;
    assert!(response.contains("\"name\":\"zvec-grep\""));
    let session = response
        .lines()
        .find_map(|line| {
            line.strip_prefix("mcp-session-id:")
                .map(str::trim)
                .map(str::to_owned)
        })
        .ok_or("initialize response did not contain mcp-session-id")?;

    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });
    let _ = post_json(port, Some(&session), &initialized.to_string())?;
    let list = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    let response = post_json(port, Some(&session), &list.to_string())?;
    assert!(response.contains("zvec_grep_search"));
    assert!(!response.contains("zvec_grep_index"));
    assert!(!response.contains("zvec_grep_rg"));
    assert!(response.contains("\"maximum\":50"));

    let call = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "zvec_grep_search",
            "arguments": {
                "root": home.path(),
                "query": "daemon lifecycle"
            }
        }
    });
    let response = post_json(port, Some(&session), &call.to_string())?;
    assert!(response.contains("error[ZG.ENGINE.NOT_FOUND]"));
    assert!(response.contains("workspace index at"));
    assert!(response.contains("no workspace manifest was found"));
    assert!(response.contains("\"isError\":true"));

    // CLI administration uses the typed daemon protocol rather than the
    // public MCP toolset, so status remains available with the agent profile.
    let cli_status = Command::new(&guard.binary)
        .args(["--status", "--mode", "server", "--home"])
        .arg(&guard.home)
        .arg(home.path())
        .output()?;
    assert_command_success(&cli_status);
    let cli_stdout = String::from_utf8_lossy(&cli_status.stdout);
    assert!(cli_stdout.contains("Workspace index: uninitialized"));
    assert!(cli_stdout.contains(&format!("Root: {}", home.path().display())));

    let output = guard.stop()?;
    assert_command_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Server: stopped"));
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn full_toolset_exposes_lifecycle_tools_and_runs_managed_rg() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    std::fs::write(
        workspace.path().join("sample.txt"),
        "resident workspace manager\n",
    )?;
    // The mock provider requires the same explicit workspace consent as real providers.
    let signing_key = home.path().join("authorization.key");
    let consent = Command::new(&binary)
        .env("ZVEC_GREP_AUTHORIZATION_KEY_FILE", &signing_key)
        .args([
            "--auth",
            "grant",
            path_text(workspace.path())?,
            "--capability",
            "embedding",
            "--scope",
            "workspace",
            "--embedding",
            "qwen/text-embedding-v4",
            "--endpoint",
            &format!("http://{}/embeddings", embedding.address),
        ])
        .output()?;
    assert_command_success(&consent);
    let (mut guard, output) = start_server(&binary, &home, "full", None, |command| {
        command
            .env("ZVEC_GREP_API_KEY", "local-test-key")
            .env("ZVEC_GREP_AUTHORIZATION_KEY_FILE", &signing_key);
    })?;
    let port = guard.listen.parse::<SocketAddr>()?.port();
    assert!(String::from_utf8_lossy(&output.stdout).contains("MCP toolset: full"));

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "zg-full-test", "version": "1" }
        }
    });
    let response = post_json(port, None, &initialize.to_string())?;
    let session = response
        .lines()
        .find_map(|line| {
            line.strip_prefix("mcp-session-id:")
                .map(str::trim)
                .map(str::to_owned)
        })
        .ok_or("initialize response did not contain mcp-session-id")?;
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });
    let _ = post_json(port, Some(&session), &initialized.to_string())?;

    let list = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    let response = post_json(port, Some(&session), &list.to_string())?;
    for name in [
        "zvec_grep_search",
        "zvec_grep_index",
        "zvec_grep_index_drop",
        "zvec_grep_rg",
        "zvec_grep_index_status",
        "zvec_grep_server_status",
    ] {
        assert!(response.contains(name), "full toolset is missing {name}");
    }

    let rg = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "zvec_grep_rg",
            "arguments": {
                "root": workspace.path(),
                "command": "rg -F resident sample.txt"
            }
        }
    });
    let response = post_json(port, Some(&session), &rg.to_string())?;
    assert!(response.contains("matchedBy=lexical sample.txt:1"));
    assert!(response.contains("resident workspace manager"));
    assert!(response.contains("\"isError\":false"));

    for (command, has_match) in [
        ("rg -F ' resident ' sample.txt", false),
        ("rg --iglob '*.RS' resident .", false),
        ("rg -S Resident sample.txt", false),
        ("rg -S RESIDENT -i sample.txt", true),
        ("rg -e '' sample.txt", true),
    ] {
        let mut request = rg.clone();
        request["params"]["arguments"]["command"] = json!(command);
        let response = post_json(port, Some(&session), &request.to_string())?;
        assert!(
            response.contains("\"isError\":false"),
            "{command}: {response}"
        );
        assert_eq!(
            response.contains("matchedBy=lexical"),
            has_match,
            "{command}: {response}"
        );
    }

    let index = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "zvec_grep_index",
            "arguments": {
                "root": workspace.path(),
                "wait": false,
                "embedding": "qwen/text-embedding-v4",
                "endpoint": format!("http://{}/embeddings", embedding.address)
            }
        }
    });
    let response = post_json(port, Some(&session), &index.to_string())?;
    assert!(response.contains("\"state\":\"queued\""));
    assert!(response.contains("\"job_id\":"));
    assert!(!response.contains("generation-"));
    assert!(response.contains("\"isError\":false"));

    let status = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "zvec_grep_server_status",
            "arguments": {}
        }
    });
    let response = post_json(port, Some(&session), &status.to_string())?;
    assert!(response.contains("active_runtimes"));
    assert!(response.contains("\"active_runtimes\":1"));
    assert!(response.contains("structuredContent"));
    assert!(response.contains("\"isError\":false"));

    let index_status = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "zvec_grep_index_status",
            "arguments": { "root": workspace.path() }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let response = loop {
        let response = post_json(port, Some(&session), &index_status.to_string())?;
        assert!(!response.contains("\"job_state\":\"failed\""), "{response}");
        if response.contains("\"job_state\":\"succeeded\"") {
            break response;
        }
        assert!(
            Instant::now() < deadline,
            "index did not complete: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(response.contains("\"indexed\":true"), "{response}");
    assert!(response.contains("\"index_policy\":\"enabled\""));
    assert!(response.contains("\"source\":\"index\""));
    assert!(response.contains("\"runtime\":"));
    assert!(response.contains("\"isError\":false"));
    assert!(response.contains("\"indexed\":1"), "{response}");
    assert!(response.contains("\"failed\":0"), "{response}");

    let search = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "zvec_grep_search",
            "arguments": {
                "root": workspace.path(),
                "fts": "resident",
                "autoUpdate": false
            }
        }
    });
    let response = post_json(port, Some(&session), &search.to_string())?;
    assert!(response.contains("sample.txt"), "{response}");
    assert!(
        response.contains("resident workspace manager"),
        "{response}"
    );
    assert!(response.contains("\"isError\":false"), "{response}");
    assert!(response.contains("freshness: possibly_stale"), "{response}");
    assert!(
        response.contains("results: served_from_current_index"),
        "{response}"
    );
    assert!(response.contains("background_refresh: off"), "{response}");

    std::fs::write(
        workspace.path().join("fresh.txt"),
        "freshnessbarrier newly created content\n",
    )?;
    let wait_search = json!({
        "jsonrpc": "2.0", "id": 8, "method": "tools/call",
        "params": { "name": "zvec_grep_search", "arguments": {
            "root": workspace.path(), "fts": "freshnessbarrier",
            "autoUpdate": false, "freshness": "wait_for_fresh"
        } }
    });
    // Resident Wait drains delivered notifications; OS delivery itself is
    // asynchronous. Controlled runtime tests verify waiting for in-flight jobs.
    let deadline = Instant::now() + Duration::from_secs(15);
    let response = loop {
        let response = post_json(port, Some(&session), &wait_search.to_string())?;
        assert!(response.contains("\"isError\":false"), "{response}");
        if response.contains("fresh.txt") {
            break response;
        }
        assert!(
            Instant::now() < deadline,
            "watcher did not deliver change: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(response.contains("freshness: fresh"), "{response}");
    assert!(!response.contains("background_refresh:"), "{response}");
    assert!(
        !response.contains("results: served_from_current_index"),
        "{response}"
    );
    assert!(response.contains("\"isError\":false"), "{response}");

    let background_search = json!({
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": { "name": "zvec_grep_search", "arguments": {
            "root": workspace.path(), "fts": "freshnessbarrier"
        } }
    });
    let response = post_json(port, Some(&session), &background_search.to_string())?;
    assert!(response.contains("fresh.txt"), "{response}");
    assert!(response.contains("freshness: possibly_stale"), "{response}");
    assert!(
        response.contains("results: served_from_current_index"),
        "{response}"
    );
    assert!(
        !response.contains("freshness: served_from_current_index"),
        "{response}"
    );
    // A native watcher can deliver delayed or duplicate notifications after the
    // preceding fresh query. Controlled runtime tests cover the quiet idle case.
    assert!(
        response.contains("background_refresh: idle")
            || response.contains("background_refresh: scheduled"),
        "{response}"
    );
    assert!(response.contains("\"isError\":false"), "{response}");

    let output = guard.stop()?;
    assert_command_success(&output);
    Ok(())
}

#[test]
fn new_daemon_defaults_to_agent_without_a_toolset() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let (mut guard, output) = start_on_available_port(&binary, &home, None, |listen| {
        server_start_output(
            Command::new(&binary)
                .env_remove("ZVEC_GREP_MCP_TOOLSET")
                .args(["--server", "on", "--home"])
                .arg(home.path())
                .args(["--listen", listen]),
        )
    })?;
    assert!(String::from_utf8_lossy(&output.stdout).contains("MCP toolset: agent"));
    assert_command_success(&guard.stop()?);
    let log = std::fs::read_to_string(home.path().join("daemon").join("logs").join("server.log"))?;
    assert!(
        log.lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|record| record["fields"]["message"] == "zvec-grep daemon ready")
        }),
        "{log}"
    );
    Ok(())
}

#[test]
fn default_connections_reuse_either_toolset_and_explicit_conflicts_fail()
-> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    for profile in ["agent", "full"] {
        let home = TempDir::new()?;
        let (mut guard, _) = start_server(&binary, &home, profile, None, |_| {})?;
        let status = std::fs::read(home.path().join("daemon/instance.lock"))?;
        let original: serde_json::Value = serde_json::from_slice(&status)?;
        for (argument, environment, success) in [
            (None, None, true),
            (Some(profile), None, true),
            (None, Some(profile), true),
            (
                Some(profile),
                Some(if profile == "agent" { "full" } else { "agent" }),
                true,
            ),
            (
                Some(if profile == "agent" { "full" } else { "agent" }),
                None,
                false,
            ),
            (
                None,
                Some(if profile == "agent" { "full" } else { "agent" }),
                false,
            ),
        ] {
            let mut command = Command::new(&binary);
            command
                .args(["--server", "on", "--home"])
                .arg(home.path())
                .env_remove("ZVEC_GREP_MCP_TOOLSET");
            if let Some(argument) = argument {
                command.args(["--mcp-toolset", argument]);
            }
            if let Some(environment) = environment {
                command.env("ZVEC_GREP_MCP_TOOLSET", environment);
            }
            let output = command.output()?;
            assert_eq!(
                output.status.success(),
                success,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if success {
                assert!(
                    String::from_utf8_lossy(&output.stdout)
                        .contains(&format!("MCP toolset: {profile}"))
                );
            } else {
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains("before changing toolsets")
                );
            }
        }
        let mut bridge =
            StdioBridge::spawn_with_toolset(&binary, home.path(), &guard.listen, None)?;
        bridge.request(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": { "name": "toolset-reuse-test", "version": "1" } }
        }))?;
        bridge.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        let tools = bridge.request(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))?;
        assert_eq!(
            tools["result"]["tools"]
                .as_array()
                .ok_or("tool list")?
                .len(),
            if profile == "agent" { 1 } else { 6 }
        );
        bridge.close()?;
        let current: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.path().join("daemon/instance.lock"))?)?;
        assert_eq!(current["instanceToken"], original["instanceToken"]);
        assert_eq!(current["pid"], original["pid"]);
        assert_command_success(&guard.stop()?);
    }
    Ok(())
}

#[test]
fn agent_search_uses_workspace_runtime() -> Result<(), Box<dyn Error>> {
    search_uses_workspace_runtime("agent")
}

#[test]
fn full_search_uses_workspace_runtime() -> Result<(), Box<dyn Error>> {
    search_uses_workspace_runtime("full")
}

#[expect(
    clippy::too_many_lines,
    reason = "Compare both toolsets through the same MCP lifecycle"
)]
fn search_uses_workspace_runtime(toolset: &str) -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let missing = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    let endpoint = format!("http://{}/embeddings", embedding.address);
    let source = workspace.path().join("source.txt");
    std::fs::write(&source, "orchard documentation")?;
    // Build before starting the daemon so only search can activate its runtime.
    let indexed = Command::new(&binary)
        .current_dir(workspace.path())
        .env("ZVEC_GREP_HOME", home.path())
        .env(
            "ZVEC_GREP_WORKSPACE_REGISTRY",
            home.path().join("workspaces.json"),
        )
        .env("ZVEC_GREP_API_KEY", "local-test-key")
        .args([
            "--index",
            "--mode",
            "direct",
            "--allow-remote",
            "--embedding",
            "qwen/text-embedding-v4",
            "--endpoint",
            &endpoint,
        ])
        .output()?;
    assert_command_success(&indexed);
    let signing_key = home.path().join("authorization.key");
    let consent = Command::new(&binary)
        .env("ZVEC_GREP_AUTHORIZATION_KEY_FILE", &signing_key)
        .args([
            "--auth",
            "grant",
            path_text(workspace.path())?,
            "--capability",
            "embedding",
            "--scope",
            "workspace",
            "--embedding",
            "qwen/text-embedding-v4",
            "--endpoint",
            &endpoint,
        ])
        .output()?;
    assert_command_success(&consent);
    let (mut guard, _) = start_server(&binary, &home, toolset, None, |command| {
        command
            .env("ZVEC_GREP_AUTHORIZATION_KEY_FILE", &signing_key)
            .env("ZVEC_GREP_API_KEY", "local-test-key")
            .env(
                "ZVEC_GREP_WORKSPACE_REGISTRY",
                home.path().join("workspaces.json"),
            );
    })?;
    let port = guard.listen.parse::<SocketAddr>()?.port();
    let response = post_json(
        port,
        None,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": { "name": "runtime-parity-test", "version": "1" } }
        })
        .to_string(),
    )?;
    let session = response
        .lines()
        .find_map(|line| line.strip_prefix("mcp-session-id:").map(str::trim))
        .ok_or("missing MCP session")?;
    post_json(
        port,
        Some(session),
        &json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        })
        .to_string(),
    )?;
    let search = |root: &Path, query: &str, freshness: &str, auto_update: bool| {
        post_json(
            port,
            Some(session),
            &json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "zvec_grep_search", "arguments": {
                    "root": root, "fts": query, "freshness": freshness,
                    "autoUpdate": auto_update
                } }
            })
            .to_string(),
        )
    };
    for (freshness, auto_update) in [
        ("eventual", false),
        ("eventual", true),
        ("wait_for_fresh", false),
    ] {
        let response = search(missing.path(), "orchard", freshness, auto_update)?;
        assert!(response.contains("\"isError\":true"), "{response}");
        assert!(!missing.path().join(".zvec-grep").exists());
    }
    std::fs::write(&source, "vineyard documentation")?;
    let response = search(workspace.path(), "orchard", "eventual", false)?;
    assert!(response.contains("source.txt"), "{response}");
    assert!(response.contains("background_refresh: off"), "{response}");
    let response = search(workspace.path(), "orchard", "eventual", true)?;
    assert!(response.contains("source.txt"), "{response}");
    assert!(
        response.contains("background_refresh: scheduled"),
        "{response}"
    );
    // Poll without requesting more refreshes: the first eventual search must do the work.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let response = search(workspace.path(), "vineyard", "eventual", false)?;
        if response.contains("source.txt") && response.contains("\"isError\":false") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background refresh did not complete: {response}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let before_edit = embedding.requests.load(Ordering::SeqCst);
    std::fs::write(&source, "harvest documentation")?;
    // Wait covers delivered watcher events; OS delivery can lag behind the write.
    embedding.wait_for_request_after(before_edit);
    let response = search(workspace.path(), "harvest", "wait_for_fresh", false)?;
    assert!(response.contains("source.txt"), "{response}");
    assert!(response.contains("freshness: fresh"), "{response}");
    assert!(response.contains("\"isError\":false"), "{response}");
    // Once activated, the watcher must refresh later edits even with autoUpdate off.
    std::fs::write(&source, "autumn documentation")?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let response = search(workspace.path(), "autumn", "eventual", false)?;
        if response.contains("source.txt") && response.contains("\"isError\":false") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "watcher did not refresh: {response}\n{}",
            log_tail(
                &home.path().join("daemon").join("logs").join("server.log"),
                0
            )
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    embedding.fail.store(true, Ordering::Release);
    let before_failure = embedding.requests.load(Ordering::SeqCst);
    std::fs::write(&source, "winter documentation")?;
    embedding.wait_for_request_after(before_failure);
    let response = search(workspace.path(), "winter", "wait_for_fresh", false)?;
    assert!(
        response.contains("\"isError\":true"),
        "failed refresh reported success: {response}"
    );
    assert!(!response.contains("freshness: fresh"), "{response}");
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn concurrent_stdio_bootstraps_share_one_resident_daemon() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let (mut guard, mut bridges) = start_on_available_port(&binary, &home, None, |listen| {
        let mut bridges = (0..4)
            .map(|_| StdioBridge::spawn(&binary, home.path(), listen))
            .collect::<Result<Vec<_>, _>>()?;

        for (index, bridge) in bridges.iter_mut().enumerate() {
            let initialize = json!({
                "jsonrpc": "2.0",
                "id": index + 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "zg-stdio-test", "version": "1" }
                }
            });
            let response = bridge.request(&initialize)?;
            assert_eq!(response["result"]["serverInfo"]["name"], "zvec-grep");
            bridge.notify(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {}
            }))?;
        }
        Ok(bridges)
    })?;

    let list = bridges[0].request(&json!({
        "jsonrpc": "2.0",
        "id": 100,
        "method": "tools/list",
        "params": {}
    }))?;
    let tools = list["result"]["tools"]
        .as_array()
        .ok_or("tools/list did not return an array")?;
    assert_eq!(tools.len(), 6);

    for bridge in bridges {
        bridge.close()?;
    }

    let status = Command::new(&binary)
        .args(["--server", "status", "--home"])
        .arg(home.path())
        .output()?;
    assert_command_success(&status);
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("Server: ready"));
    assert!(stdout.contains("MCP toolset: full"));

    let output = guard.stop()?;
    assert_command_success(&output);
    Ok(())
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Exercise update, rebuild and drop in one resident daemon lifecycle"
)]
fn direct_writes_retire_daemon_read_sessions() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    let endpoint = format!("http://{}/embeddings", embedding.address);
    let direct = |extra: &[&str]| {
        let mut command = Command::new(&binary);
        command
            .current_dir(workspace.path())
            .env("ZVEC_GREP_HOME", home.path())
            .env(
                "ZVEC_GREP_WORKSPACE_REGISTRY",
                home.path().join("workspaces.json"),
            )
            .env("ZVEC_GREP_API_KEY", "local-test-key")
            .args(["--index", "--mode", "direct"])
            .args(extra);
        if !extra.contains(&"--drop") {
            command.arg("--allow-remote");
        }
        command.output()
    };
    std::fs::write(workspace.path().join("source.txt"), "orchard documentation")?;
    assert_command_success(&direct(&[
        "--embedding",
        "qwen/text-embedding-v4",
        "--endpoint",
        &endpoint,
    ])?);
    // CLI writers are separate processes, not children of the native cache owner.
    // This also avoids inheriting the native library's open lock descriptors.
    let (mut guard, _) = start_server(&binary, &home, "full", None, |command| {
        command.env(
            "ZVEC_GREP_WORKSPACE_REGISTRY",
            home.path().join("workspaces.json"),
        );
    })?;
    let port = guard.listen.parse::<SocketAddr>()?.port();
    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "read-cache-test", "version": "1" } }
    });
    let response = post_json(port, None, &initialize.to_string())?;
    let session = response
        .lines()
        .find_map(|line| line.strip_prefix("mcp-session-id:").map(str::trim))
        .ok_or("missing MCP session")?;
    post_json(
        port,
        Some(session),
        &json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        })
        .to_string(),
    )?;
    let search = |query: &str| {
        post_json(
            port,
            Some(session),
            &json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "zvec_grep_search", "arguments": {
                    "root": workspace.path(), "fts": query, "autoUpdate": false
                } }
            })
            .to_string(),
        )
    };
    for _ in 0..2 {
        let response = search("orchard")?;
        assert!(
            response.contains("source.txt") && response.contains("\"isError\":false"),
            "{response}"
        );
    }
    std::fs::write(
        workspace.path().join("source.txt"),
        "vineyard documentation",
    )?;
    assert_command_success(&direct(&[])?);
    let response = search("vineyard")?;
    assert!(
        response.contains("source.txt") && response.contains("\"isError\":false"),
        "{response}"
    );
    let response = search("orchard")?;
    assert!(
        !response.contains("source.txt"),
        "old index contents remained cached: {response}"
    );

    let manifest_path = workspace.path().join(".zvec-grep/manifest.json");
    let before: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    assert_command_success(&direct(&["--rebuild"])?);
    let after: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    assert_ne!(before["storageGeneration"], after["storageGeneration"]);
    let response = search("vineyard")?;
    assert!(
        response.contains("source.txt") && response.contains("\"isError\":false"),
        "{response}"
    );
    assert_command_success(&direct(&["--drop", "--yes"])?);
    let response = search("vineyard")?;
    assert!(
        response.contains("\"isError\":true"),
        "dropped index remained cached: {response}"
    );
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn indexed_fragment_coordinates_survive_direct_server_and_mcp() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    // Exceed the configured model's input budget while keeping only two source lines.
    let source = format!("# Heading\n{} linenumberprobe\n", "x".repeat(20_000));
    std::fs::write(workspace.path().join("sample.md"), source)?;
    let registry = home.path().join("workspaces.json");
    let index = Command::new(&binary)
        .current_dir(workspace.path())
        .env("ZVEC_GREP_HOME", home.path())
        .env("ZVEC_GREP_WORKSPACE_REGISTRY", &registry)
        .env("ZVEC_GREP_API_KEY", "local-test-key")
        .args([
            "--index",
            "--mode",
            "direct",
            "--allow-remote",
            "--embedding",
            "qwen/text-embedding-v4",
            "--endpoint",
        ])
        .arg(format!("http://{}/embeddings", embedding.address))
        .output()?;
    assert_command_success(&index);
    let (mut guard, _) = start_server(&binary, &home, "full", None, |command| {
        command.env("ZVEC_GREP_WORKSPACE_REGISTRY", &registry);
    })?;
    let mut bridge = StdioBridge::spawn(&binary, home.path(), &guard.listen)?;
    bridge.request(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "source-coordinate-test", "version": "1" } }
    }))?;
    bridge.notify(
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }),
    )?;
    for preview in ["short", "full"] {
        let mut outputs = Vec::new();
        for mode in ["direct", "server"] {
            let reply = Command::new(&binary)
                .current_dir(workspace.path())
                .env("ZVEC_GREP_HOME", home.path())
                .env("ZVEC_GREP_WORKSPACE_REGISTRY", &registry)
                .args([
                    "--mode",
                    mode,
                    "--fts",
                    "linenumberprobe",
                    "--refresh",
                    "off",
                    "--preview",
                    preview,
                ])
                .output()?;
            assert_command_success(&reply);
            let text = String::from_utf8(reply.stdout)?;
            assert!(text.contains("\n  2: xxx"), "{mode} {preview}: {text}");
            assert!(!text.contains("\n  1: xxx"), "{mode} {preview}: {text}");
            outputs.push(text);
        }
        assert_eq!(outputs[0], outputs[1], "direct/server {preview}");
        let reply = bridge.request(&json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "zvec_grep_search", "arguments": {
                "root": workspace.path(), "fts": "linenumberprobe", "autoUpdate": false, "preview": preview
            } }
        }))?;
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .ok_or("missing source preview")?;
        assert!(
            text.contains("matched: 2\nsource:\n2\txxx"),
            "{preview}: {text}"
        );
        assert!(text.ends_with("\n3\t"), "{preview}: {text}");
    }
    bridge.close()?;
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn stdio_remote_consent_controls_transmission_and_persistence() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    std::fs::write(
        workspace.path().join("sample.md"),
        "# Consent\nremote consent fixture\n",
    )?;
    let signing_key = home.path().join("authorization.key");
    let (mut guard, _) = start_server(&binary, &home, "full", None, |command| {
        command
            .env("ZVEC_GREP_AUTHORIZATION_KEY_FILE", &signing_key)
            .env("ZVEC_GREP_API_KEY", "test-key");
    })?;
    let mut bridge = StdioBridge::spawn(&binary, home.path(), &guard.listen)?;
    bridge.request(
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {"elicitation": {"form": {}}},
            "clientInfo": {"name": "consent-test", "version": "1"}
        }}),
    )?;
    bridge.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
    let index = |id| {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": "zvec_grep_index", "arguments": {
                "root": workspace.path(), "embedding": "qwen/text-embedding-v4",
                "endpoint": format!("http://{}/embeddings", embedding.address), "apiKey": "test-key",
                "wait": true, "debug": true
            }
        }})
    };
    let (declined, prompts) = bridge.request_with_consent(&index(2), "cancel")?;
    assert_eq!(prompts, 1);
    assert_eq!(declined["result"]["isError"], true);
    assert_eq!(embedding.requests.load(Ordering::SeqCst), 0);
    assert!(!signing_key.exists());
    let (indexed, prompts) = bridge.request_with_consent(&index(3), "once")?;
    assert_eq!(prompts, 1);
    assert_eq!(indexed["result"]["isError"], false, "{indexed}");
    assert_eq!(
        indexed["result"]["structuredContent"]["state"], "succeeded",
        "{indexed}"
    );
    assert!(indexed["result"]["structuredContent"]["debug"]["timings"].is_array());
    let indexed_requests = embedding.requests.load(Ordering::SeqCst);
    assert!(indexed_requests > 0);
    assert!(
        !signing_key.exists(),
        "one-operation consent must not create a signing key"
    );
    let search = |id| {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": "zvec_grep_search", "arguments": {"root": workspace.path(), "query": "consent", "autoUpdate": false}
        }})
    };
    let (fts, prompts) = bridge.request_with_consent(&search(4), "fts_only")?;
    assert_eq!(prompts, 1);
    assert_eq!(fts["result"]["isError"], false, "{fts}");
    assert_eq!(embedding.requests.load(Ordering::SeqCst), indexed_requests);
    let (persisted, prompts) = bridge.request_with_consent(&index(5), "workspace")?;
    assert_eq!(prompts, 1);
    assert_eq!(persisted["result"]["isError"], false, "{persisted}");
    assert!(signing_key.exists());
    let (queried, prompts) = bridge.request_with_consent(&search(6), "cancel")?;
    assert_eq!(prompts, 0, "valid workspace grants must skip elicitation");
    assert_eq!(queried["result"]["isError"], false, "{queried}");
    assert!(embedding.requests.load(Ordering::SeqCst) > indexed_requests);
    bridge.close()?;
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn direct_index_failures_match_readiness_and_recover() -> Result<(), Box<dyn Error>> {
    index_failures_match_readiness_and_recover("direct")
}

#[test]
fn server_index_failures_match_readiness_and_recover() -> Result<(), Box<dyn Error>> {
    index_failures_match_readiness_and_recover("server")
}

fn index_failures_match_readiness_and_recover(mode: &str) -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start_with_failure_status(400)?;
    let endpoint = format!("http://{}/embeddings", embedding.address);
    let configure = |command: &mut Command| {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ZVEC_GREP_") {
                command.env_remove(key);
            }
        }
        command
            .current_dir(workspace.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ZVEC_GREP_HOME", home.path())
            .env(
                "ZVEC_GREP_WORKSPACE_REGISTRY",
                home.path().join("workspaces.json"),
            )
            .env_remove("DASHSCOPE_API_KEY")
            .env_remove("QWEN_API_KEY");
    };
    let run = |args: &[&str]| {
        let mut command = Command::new(&binary);
        configure(&mut command);
        command
            .args(args)
            .args(["--mode", mode, "--no-color"])
            .output()
    };
    let mut guard = if mode == "server" {
        Some(start_server(&binary, &home, "full", None, configure)?.0)
    } else {
        None
    };
    let good = workspace.path().join("good.txt");
    let broken = workspace.path().join("broken.txt");
    std::fs::write(&good, "Stable orchard baseline.\n")?;
    // An incomplete BOM-marked UTF-16 code unit must fail extraction.
    std::fs::write(&broken, [255_u8, 254, 255])?;
    let index_args = [
        "--index",
        "--embedding",
        "qwen/text-embedding-v4",
        "--endpoint",
        &endpoint,
        "--api-key",
        "local-test-key",
        "--allow-remote",
    ];
    for failed_path in ["broken.txt", "good.txt"] {
        let failed = run(&index_args)?;
        let stdout = String::from_utf8_lossy(&failed.stdout);
        let stderr = String::from_utf8_lossy(&failed.stderr);
        assert_eq!(failed.status.code(), Some(1), "{mode}: {stdout}\n{stderr}");
        assert!(
            stdout.starts_with("Workspace index: failed\n"),
            "{mode}: {stdout}\n{stderr}"
        );
        assert!(!stdout.contains("Workspace index: ready"), "{stdout}");
        assert!(stdout.contains("failed=1"), "{stdout}");
        assert!(
            stderr.contains("indexing completed with 1 failed file"),
            "{stderr}"
        );
        assert!(
            stdout.contains(&format!("Failed: {failed_path}:")),
            "{stdout}"
        );
        let status = run(&["--status", "--check-ready"])?;
        assert_eq!(status.status.code(), Some(1));
        let stdout = String::from_utf8_lossy(&status.stdout);
        assert!(stdout.contains("indexed=1 pending=1 failed=1"), "{stdout}");
        assert!(stdout.contains("Workspace index: failed"), "{stdout}");
        std::fs::write(&broken, "Recovered readable nebula documentation.\n")?;
        embedding.fail.store(false, Ordering::Release);
        let recovered = run(&index_args)?;
        assert_command_success(&recovered);
        let stdout = String::from_utf8_lossy(&recovered.stdout);
        assert!(stdout.starts_with("Workspace index: ready\n"), "{stdout}");
        assert!(stdout.contains("failed=0"), "{stdout}");
        assert_command_success(&run(&["--status", "--check-ready"])?);
        if failed_path == "broken.txt" {
            std::fs::write(&good, "Modified orchard baseline.\n")?;
            embedding.fail.store(true, Ordering::Release);
        }
    }
    assert!(embedding.requests.load(Ordering::Acquire) > 0);
    if let Some(guard) = &mut guard {
        assert_command_success(&guard.stop()?);
    }
    Ok(())
}

struct EmbeddingServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    requests: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
}

impl EmbeddingServer {
    fn wait_for_request_after(&self, previous: usize) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self.requests.load(Ordering::SeqCst) <= previous {
            assert!(
                Instant::now() < deadline,
                "watcher did not submit the edited file"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn start() -> std::io::Result<Self> {
        Self::start_with_failure_status(401)
    }

    fn start_with_failure_status(failure_status: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let worker = std::thread::spawn({
            let stop = Arc::clone(&stop);
            let requests = Arc::clone(&requests);
            let fail = Arc::clone(&fail);
            move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    requests.fetch_add(1, Ordering::SeqCst);
                    respond_embedding(
                        stream.expect("mock embedding connection"),
                        fail.load(Ordering::Acquire),
                        failure_status,
                    )
                    .expect("mock embedding response");
                }
            }
        });
        Ok(Self {
            address,
            stop,
            worker: Some(worker),
            requests,
            fail,
        })
    }
}

impl Drop for EmbeddingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                assert!(result.is_ok(), "mock embedding server failed");
            }
        }
    }
}

fn respond_embedding(
    mut stream: TcpStream,
    fail: bool,
    failure_status: u16,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        if line == "\r\n" {
            break;
        }
        if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = Some(length.trim().parse::<usize>().expect("content length"));
        }
    }
    let mut body = vec![0; content_length.expect("request body length")];
    reader.read_exact(&mut body)?;
    if fail {
        return write!(
            stream,
            "HTTP/1.1 {failure_status} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            if failure_status == 401 {
                "Unauthorized"
            } else {
                "Bad Request"
            }
        );
    }
    let request: serde_json::Value = serde_json::from_slice(&body)?;
    let dimension = usize::try_from(request["dimensions"].as_u64().expect("dimension"))
        .expect("usize dimension");
    let mut vector = vec![0.0_f32; dimension];
    vector[0] = 1.0;
    let data = request["input"]
        .as_array()
        .expect("text inputs")
        .iter()
        .enumerate()
        .map(|(index, _)| json!({ "index": index, "embedding": vector }))
        .collect::<Vec<_>>();
    let response = serde_json::to_vec(&json!({ "data": data }))?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.len()
    )?;
    stream.write_all(&response)
}

fn available_port() -> Result<u16, std::io::Error> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn post_json(port: u16, session: Option<&str>, body: &str) -> Result<String, Box<dyn Error>> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    // A wait_for_fresh request includes indexing work, which can exceed five
    // seconds on slower CI runners even with a local embedding provider.
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let session_header = session.map_or_else(String::new, |value| {
        format!("Mcp-Session-Id: {value}\r\nMCP-Protocol-Version: 2025-11-25\r\n")
    });
    write!(
        stream,
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{session_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

fn path_text(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| "temporary path is not valid UTF-8".into())
}

fn assert_command_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn token_file_protects_daemon_requests_and_is_forwarded_to_child() -> Result<(), Box<dyn Error>> {
    let home = TempDir::new()?;
    let token_file = home.path().join("token.txt");
    std::fs::write(&token_file, "test-token-012345678901234567890123456789\n")?;
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let (mut guard, _) = start_server(&binary, &home, "agent", Some(&token_file), |command| {
        command
            .env_remove("ZVEC_GREP_SERVER_TOKEN")
            .env_remove("ZVEC_GREP_SERVER_TOKEN_FILE");
    })?;
    let listen = &guard.listen;
    let mut connection = TcpStream::connect(listen)?;
    connection.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        connection,
        "POST /admin/execute HTTP/1.1\r\nHost: {listen}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    )?;
    let mut response = String::new();
    connection.read_to_string(&mut response)?;
    assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    let workspace = TempDir::new()?;
    let status = Command::new(&binary)
        .current_dir(workspace.path())
        .args(["--status", "--mode", "server", "--home"])
        .arg(home.path())
        .env_remove("ZVEC_GREP_SERVER_TOKEN")
        .env("ZVEC_GREP_SERVER_TOKEN_FILE", &token_file)
        .output()?;
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stopped = Command::new(&binary)
        .args(["--server", "off", "--home"])
        .arg(home.path())
        .arg("--token-file")
        .arg(&token_file)
        .env_remove("ZVEC_GREP_SERVER_TOKEN")
        .output()?;
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    guard.active = false;
    Ok(())
}
