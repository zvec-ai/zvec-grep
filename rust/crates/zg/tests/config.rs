use serde_json::json;
use std::{
    fs,
    net::TcpListener,
    process::{Command, Output},
};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    user: TempDir,
}
impl Fixture {
    fn new() -> Self {
        Self {
            root: TempDir::new().expect("workspace"),
            user: TempDir::new().expect("user home"),
        }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_zg"));
        command
            .current_dir(self.root.path())
            .env("HOME", self.user.path())
            .env("USERPROFILE", self.user.path())
            .env("ZVEC_GREP_HOME", self.user.path().join("runtime"));
        for key in [
            "ZVEC_GREP_DEVICE",
            "ZVEC_GREP_EMBEDDING",
            "ZVEC_GREP_API_KEY",
            "ZVEC_GREP_ENDPOINT",
            "ZVEC_GREP_MODEL_CACHE",
            "ZVEC_GREP_MODE",
        ] {
            command.env_remove(key);
        }
        command.args(args);
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run CLI")
    }
    fn success(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn write_config(&self, config: &serde_json::Value) {
        let directory = self.user.path().join(".zvec-grep");
        fs::create_dir_all(&directory).expect("global config directory");
        fs::write(
            directory.join("config.json"),
            serde_json::to_vec(&config).expect("global config JSON"),
        )
        .expect("global config");
    }
}

struct ServerCleanup<'a>(&'a Fixture);

impl Drop for ServerCleanup<'_> {
    fn drop(&mut self) {
        let _ = self.0.run(&["--server", "off"]);
    }
}

fn available_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("available listen port")
        .local_addr()
        .expect("listen address")
        .port()
}

#[test]
fn client_mode_uses_cli_then_environment_then_global_config() {
    let fixture = Fixture::new();
    fixture.write_config(&json!({"version": 1, "client": {"mode": "server"}}));

    let configured = fixture.run(&["--status"]);
    assert!(
        !configured.status.success(),
        "global server mode should require a running daemon: {}",
        String::from_utf8_lossy(&configured.stdout)
    );
    assert!(String::from_utf8_lossy(&configured.stderr).contains("resident daemon is not ready"));
    fixture.success(&["--status", "--mode", "direct"]);
    fixture.success(&["--status", "--mode", "auto"]);

    let empty_environment = fixture
        .command(&["--status"])
        .env("ZVEC_GREP_MODE", "")
        .output()
        .expect("empty environment mode");
    assert!(
        !empty_environment.status.success(),
        "empty environment mode should defer to global server mode"
    );
    assert!(
        String::from_utf8_lossy(&empty_environment.stderr).contains("resident daemon is not ready")
    );

    let invalid_environment = fixture
        .command(&["--status"])
        .env("ZVEC_GREP_MODE", "invalid")
        .output()
        .expect("invalid environment mode");
    assert!(!invalid_environment.status.success());
    assert!(
        String::from_utf8_lossy(&invalid_environment.stderr)
            .contains("ZVEC_GREP_MODE must be direct, server, or auto")
    );
    let explicit_over_invalid_environment = fixture
        .command(&["--status", "--mode", "direct"])
        .env("ZVEC_GREP_MODE", "invalid")
        .output()
        .expect("explicit mode over invalid environment");
    assert!(
        explicit_over_invalid_environment.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit_over_invalid_environment.stderr)
    );

    let environment = fixture
        .command(&["--status"])
        .env("ZVEC_GREP_MODE", "direct")
        .output()
        .expect("environment mode");
    assert!(
        environment.status.success(),
        "{}",
        String::from_utf8_lossy(&environment.stderr)
    );

    fixture.write_config(&json!({"version": 1, "client": {"mode": "direct"}}));
    let environment = fixture
        .command(&["--status"])
        .env("ZVEC_GREP_MODE", "server")
        .output()
        .expect("environment mode");
    assert!(!environment.status.success());
    assert!(String::from_utf8_lossy(&environment.stderr).contains("resident daemon is not ready"));
    let explicit = fixture
        .command(&["--status", "--mode", "direct"])
        .env("ZVEC_GREP_MODE", "server")
        .output()
        .expect("explicit mode");
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
}

#[test]
fn force_direct_uses_resolved_environment_mode() {
    let fixture = Fixture::new();
    fixture.write_config(&json!({"version": 1, "client": {"mode": "server"}}));

    let direct = fixture
        .command(&["--force-direct", "--json", "needle"])
        .env("ZVEC_GREP_MODE", "direct")
        .output()
        .expect("environment direct mode");
    assert!(!direct.status.success());
    assert!(
        String::from_utf8_lossy(&direct.stderr).contains("--json is not supported"),
        "stderr:\n{}",
        String::from_utf8_lossy(&direct.stderr)
    );

    let server = fixture
        .command(&["--force-direct", "--json", "needle"])
        .env("ZVEC_GREP_MODE", "server")
        .output()
        .expect("environment server mode");
    assert!(!server.status.success());
    assert!(
        String::from_utf8_lossy(&server.stderr).contains("--force-direct requires --mode direct"),
        "stderr:\n{}",
        String::from_utf8_lossy(&server.stderr)
    );
}

#[test]
fn force_direct_uses_resolved_global_mode() {
    let fixture = Fixture::new();
    fixture.write_config(&json!({"version": 1, "client": {"mode": "direct"}}));

    let direct = fixture.run(&["--force-direct", "--json", "needle"]);
    assert!(!direct.status.success());
    assert!(
        String::from_utf8_lossy(&direct.stderr).contains("--json is not supported"),
        "stderr:\n{}",
        String::from_utf8_lossy(&direct.stderr)
    );

    fixture.write_config(&json!({"version": 1, "client": {"mode": "server"}}));
    let server = fixture.run(&["--force-direct", "--json", "needle"]);
    assert!(!server.status.success());
    assert!(
        String::from_utf8_lossy(&server.stderr).contains("--force-direct requires --mode direct"),
        "stderr:\n{}",
        String::from_utf8_lossy(&server.stderr)
    );
}

#[test]
fn server_on_uses_global_listen_address_unless_overridden() {
    let fixture = Fixture::new();
    let _cleanup = ServerCleanup(&fixture);
    let configured_port = available_port();
    let mut explicit_port = available_port();
    while explicit_port == configured_port {
        explicit_port = available_port();
    }
    fixture.write_config(&json!({
        "version": 1,
        "server": {"host": "localhost", "port": configured_port}
    }));

    let configured = fixture.success(&["--server", "on"]);
    assert!(
        String::from_utf8_lossy(&configured.stdout)
            .contains(&format!("URL: http://127.0.0.1:{configured_port}/mcp")),
        "{}",
        String::from_utf8_lossy(&configured.stdout)
    );
    fixture.success(&["--server", "off"]);

    fixture.write_config(&json!({
        "version": 1,
        "server": {"host": "0.0.0.0", "port": configured_port}
    }));
    let invalid_listen = fixture.run(&["--server", "on"]);
    assert!(!invalid_listen.status.success());
    assert!(
        String::from_utf8_lossy(&invalid_listen.stderr).contains("must be loopback"),
        "{}",
        String::from_utf8_lossy(&invalid_listen.stderr)
    );

    let explicit_listen = format!("127.0.0.1:{explicit_port}");
    let explicit = fixture.success(&["--server", "on", "--listen", &explicit_listen]);
    assert!(
        String::from_utf8_lossy(&explicit.stdout)
            .contains(&format!("URL: http://127.0.0.1:{explicit_port}/mcp")),
        "{}",
        String::from_utf8_lossy(&explicit.stdout)
    );
}

#[test]
fn server_on_uses_bracketed_global_ipv6_host_when_available() {
    let Ok(listener) = TcpListener::bind("[::1]:0") else {
        return;
    };
    let port = listener.local_addr().expect("IPv6 listen address").port();
    drop(listener);

    let fixture = Fixture::new();
    let _cleanup = ServerCleanup(&fixture);
    fixture.write_config(&json!({
        "version": 1,
        "server": {"host": "[::1]", "port": port}
    }));
    let started = fixture.success(&["--server", "on"]);
    assert!(
        String::from_utf8_lossy(&started.stdout).contains(&format!("URL: http://[::1]:{port}/mcp")),
        "{}",
        String::from_utf8_lossy(&started.stdout)
    );
}

#[test]
fn config_merges_settings_and_index_consumes_model_defaults() {
    let fixture = Fixture::new();
    let output = fixture.success(&[
        "--config",
        "provider",
        "set",
        "qwen",
        "--api-key",
        "test-secret",
    ]);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("test-secret"));
    fixture.success(&[
        "--config",
        "model",
        "set",
        "local/potion-code-16m-v2",
        "--device",
        "cpu",
        "--default",
    ]);
    fixture.success(&[
        "--config",
        "model",
        "set",
        "qwen/text-embedding-v4",
        "--endpoint",
        "https://example.test/embeddings",
    ]);
    let config_path = fixture.user.path().join(".zvec-grep/config.json");
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).expect("config")).expect("JSON");
    assert_eq!(config["providers"]["qwen"]["apiKey"], "test-secret");
    assert_eq!(
        config["models"]["local/potion-code-16m-v2"]["device"],
        "cpu"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(config_path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    fixture.success(&["--index", "--mode", "direct", "--model-cache", "cache"]);
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.root.path().join(".zvec-grep/manifest.json")).expect("manifest"),
    )
    .expect("JSON");
    assert_eq!(
        manifest["embeddings"][0]["model"]["name"],
        "potion-code-16m-v2"
    );
    assert_eq!(
        manifest["embeddingRuntimes"]["local/potion-code-16m-v2"]["device"],
        "cpu"
    );
    let status = fixture.success(&["--status", "--mode", "direct"]);
    assert!(
        String::from_utf8_lossy(&status.stdout)
            .contains("  FTS         tokenizer=jieba filters=lowercase")
    );
}

#[test]
fn invalid_config_commands_leave_no_config_file() {
    let fixture = Fixture::new();
    for args in [
        vec![
            "--config",
            "provider",
            "set",
            "local",
            "--api-key",
            "test-key",
        ],
        vec![
            "--config",
            "model",
            "set",
            "local/potion-code-16m-v2",
            "--endpoint",
            "https://example.test",
        ],
        vec![
            "--config",
            "model",
            "set",
            "qwen/text-embedding-v4",
            "--device",
            "cpu",
        ],
        vec!["--config", "model", "set", "qwen/text-embedding-v4"],
    ] {
        assert!(!fixture.run(&args).status.success());
    }
    assert!(!fixture.user.path().join(".zvec-grep/config.json").exists());
}

#[test]
fn binary_consumes_compact_color_and_debug_options() {
    let fixture = Fixture::new();
    fs::write(fixture.root.path().join("sample.txt"), "needle\n").expect("source");
    let output = fixture.success(&[
        "--rg",
        "--compact",
        "--color",
        "always",
        "--debug",
        "needle",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("Context"));
    assert!(stdout.contains("sample.txt"));
    assert!(stdout.contains("1: needle"));
    assert!(stdout.contains("\x1b["));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Diagnostics:"));
    let output = fixture.success(&["--rg", "--color", "never", "needle"]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "sample.txt\n  1: needle\n"
    );
    let removed = fixture.run(&["--rg", "--human", "needle"]);
    assert!(!removed.status.success());
    assert!(String::from_utf8_lossy(&removed.stderr).contains("--human"));
}

#[test]
fn explicit_remote_credentials_and_consent_reach_index_execution() {
    let fixture = Fixture::new();
    fixture.success(&[
        "--index",
        "--mode",
        "direct",
        "--embedding",
        "qwen/text-embedding-v4",
        "--api-key",
        "test-key",
        "--allow-remote",
        "--endpoint",
        "https://example.test/embeddings",
    ]);
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.root.path().join(".zvec-grep/manifest.json")).expect("manifest"),
    )
    .expect("JSON");
    assert_eq!(
        manifest["embeddingRuntimes"]["qwen/text-embedding-v4"]["endpoint"],
        "https://example.test/embeddings"
    );
    fixture.success(&[
        "--index",
        "--mode",
        "direct",
        "--api-key",
        "test-key",
        "--allow-remote",
        "--endpoint",
        "https://example.test/updated",
    ]);
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.root.path().join(".zvec-grep/manifest.json")).expect("manifest"),
    )
    .expect("JSON");
    assert_eq!(
        manifest["embeddingRuntimes"]["qwen/text-embedding-v4"]["endpoint"],
        "https://example.test/updated"
    );
}
