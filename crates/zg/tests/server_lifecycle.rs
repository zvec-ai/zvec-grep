use std::{
    error::Error,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use serde_json::json;
use tempfile::TempDir;

struct ServerGuard {
    binary: PathBuf,
    home: PathBuf,
    active: bool,
}

impl ServerGuard {
    fn stop(&mut self) -> Result<Output, std::io::Error> {
        let output = Command::new(&self.binary)
            .args(["server", "off", "--home"])
            .arg(&self.home)
            .output()?;
        if output.status.success() {
            self.active = false;
        }
        Ok(output)
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = Command::new(&self.binary)
                .args(["server", "off", "--home"])
                .arg(&self.home)
                .output();
        }
    }
}

struct StdioBridge {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    reader: Option<JoinHandle<()>>,
}

impl StdioBridge {
    fn spawn(binary: &Path, home: &Path, listen: &str) -> Result<Self, Box<dyn Error>> {
        let mut child = Command::new(binary)
            .args([
                "server",
                "--stdio",
                "--home",
                path_text(home)?,
                "--listen",
                listen,
                "--mcp-toolset",
                "full",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
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
        })
    }

    fn request(
        &mut self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, Box<dyn Error>> {
        let id = request.get("id").cloned().ok_or("request has no id")?;
        let stdin = self.stdin.as_mut().ok_or("stdio bridge is closed")?;
        writeln!(stdin, "{request}")?;
        stdin.flush()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(remaining)?;
            let response = serde_json::from_str::<serde_json::Value>(&line)?;
            if response.get("id") == Some(&id) {
                return Ok(response);
            }
        }
    }

    fn notify(&mut self, notification: &serde_json::Value) -> Result<(), Box<dyn Error>> {
        let stdin = self.stdin.as_mut().ok_or("stdio bridge is closed")?;
        writeln!(stdin, "{notification}")?;
        stdin.flush()?;
        Ok(())
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
fn server_on_exposes_only_agent_search_and_off_stops_it() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let port = available_port()?;
    let listen = format!("127.0.0.1:{port}");
    let output = Command::new(&binary)
        .args([
            "server",
            "on",
            "--home",
            path_text(home.path())?,
            "--listen",
            &listen,
            "--mcp-toolset",
            "agent",
        ])
        .output()?;
    assert_command_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Server: ready"));
    assert!(stdout.contains("MCP toolset: agent"));
    let mut guard = ServerGuard {
        binary,
        home: home.path().to_owned(),
        active: true,
    };

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
        .args(["status", "--mode", "server", "--home"])
        .arg(&guard.home)
        .arg(home.path())
        .output()?;
    assert_command_success(&cli_status);
    let cli_stdout = String::from_utf8_lossy(&cli_status.stdout);
    assert!(cli_stdout.contains("Workspace index: missing"));
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
            "auth",
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
    let port = available_port()?;
    let listen = format!("127.0.0.1:{port}");
    let output = Command::new(&binary)
        .env("ZVEC_GREP_API_KEY", "local-test-key")
        .env("ZVEC_GREP_AUTHORIZATION_KEY_FILE", &signing_key)
        .args([
            "server",
            "on",
            "--home",
            path_text(home.path())?,
            "--listen",
            &listen,
            "--mcp-toolset",
            "full",
        ])
        .output()?;
    assert_command_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("MCP toolset: full"));
    let mut guard = ServerGuard {
        binary,
        home: home.path().to_owned(),
        active: true,
    };

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
    assert!(response.contains("background_refresh: off"), "{response}");

    std::fs::write(
        workspace.path().join("fresh.txt"),
        "freshnessbarrier newly created content\n",
    )?;
    let wait_search = json!({
        "jsonrpc": "2.0", "id": 8, "method": "tools/call",
        "params": { "name": "zvec_grep_search", "arguments": {
            "root": workspace.path(), "fts": "freshnessbarrier",
            "autoUpdate": false, "freshness": "wait_for_fresh",
            "endpoint": format!("http://{}/embeddings", embedding.address)
        } }
    });
    let response = post_json(port, Some(&session), &wait_search.to_string())?;
    assert!(response.contains("fresh.txt"), "{response}");
    assert!(response.contains("freshness: fresh"), "{response}");
    assert!(response.contains("background_refresh: idle"), "{response}");
    assert!(response.contains("\"isError\":false"), "{response}");

    let background_search = json!({
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": { "name": "zvec_grep_search", "arguments": {
            "root": workspace.path(), "fts": "freshnessbarrier",
            "endpoint": format!("http://{}/embeddings", embedding.address)
        } }
    });
    let response = post_json(port, Some(&session), &background_search.to_string())?;
    assert!(response.contains("fresh.txt"), "{response}");
    assert!(
        response.contains("freshness: served_from_current_index"),
        "{response}"
    );
    assert!(
        response.contains("background_refresh: scheduled"),
        "{response}"
    );
    assert!(response.contains("\"isError\":false"), "{response}");

    let output = guard.stop()?;
    assert_command_success(&output);
    Ok(())
}

#[test]
fn concurrent_stdio_bootstraps_share_one_resident_daemon() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let port = available_port()?;
    let listen = format!("127.0.0.1:{port}");
    let mut guard = ServerGuard {
        binary: binary.clone(),
        home: home.path().to_owned(),
        active: true,
    };
    let mut bridges = (0..4)
        .map(|_| StdioBridge::spawn(&binary, home.path(), &listen))
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
        .args(["server", "status", "--home"])
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

struct EmbeddingServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl EmbeddingServer {
    fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    respond_embedding(stream.expect("mock embedding connection"))
                        .expect("mock embedding response");
                }
            }
        });
        Ok(Self {
            address,
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
            if !std::thread::panicking() {
                assert!(result.is_ok(), "mock embedding server failed");
            }
        }
    }
}

fn respond_embedding(mut stream: TcpStream) -> std::io::Result<()> {
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
    let listen = format!("127.0.0.1:{}", available_port()?);
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let started = Command::new(&binary)
        .args(["server", "on", "--home"])
        .arg(home.path())
        .args(["--listen", &listen, "--token-file"])
        .arg(&token_file)
        .env_remove("ZVEC_GREP_SERVER_TOKEN")
        .env_remove("ZVEC_GREP_SERVER_TOKEN_FILE")
        .output()?;
    let mut guard = ServerGuard {
        binary: binary.clone(),
        home: home.path().to_owned(),
        active: true,
    };
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let mut connection = TcpStream::connect(&listen)?;
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
        .args(["status", "--mode", "server", "--home"])
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
        .args(["server", "off", "--home"])
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
