use std::{
    fs,
    process::{Command, Output},
};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    state: TempDir,
}
impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::tempdir().expect("authorization test fixture operation"),
            state: tempfile::tempdir().expect("authorization test fixture operation"),
        }
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_zg"))
            .current_dir(self.root.path())
            .env(
                "ZVEC_GREP_AUTHORIZATION_KEY_FILE",
                self.state.path().join("key"),
            )
            .env("ZVEC_GREP_HOME", self.state.path())
            .env_remove("ZVEC_GREP_EMBEDDING")
            .env_remove("ZVEC_GREP_ENDPOINT")
            .env_remove("ZVEC_GREP_API_KEY")
            .env_remove("DASHSCOPE_API_KEY")
            .env_remove("QWEN_API_KEY")
            .args(args)
            .output()
            .expect("authorization test fixture operation")
    }
    fn success(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("authorization test fixture operation")
    }
    fn grant(&self) {
        self.success(&[
            "auth",
            "grant",
            ".",
            "--capability",
            "embedding",
            "--scope",
            "workspace",
            "--embedding",
            "qwen/text-embedding-v4",
        ]);
    }
}

struct Stop<'a>(&'a Fixture);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.run(&["server", "off"]);
    }
}

#[test]
fn grants_are_signed_scoped_and_revocable_without_remote_requests() {
    let fixture = Fixture::new();
    assert!(
        fixture
            .success(&["auth", "status"])
            .contains("not authorized")
    );
    assert!(!fixture.root.path().join(".zvec-grep").exists());
    fixture.grant();
    assert!(
        fixture
            .success(&["auth", "status", "."])
            .contains("Model: qwen/text-embedding-v4")
    );
    assert!(
        !fixture
            .root
            .path()
            .join(".zvec-grep/manifest.json")
            .exists()
    );
    let path = fixture.root.path().join(".zvec-grep/authorization.json");
    let original = fs::read(&path).expect("authorization test fixture operation");
    let other = Fixture::new();
    fs::create_dir_all(other.root.path().join(".zvec-grep"))
        .expect("authorization test fixture operation");
    fs::copy(
        fixture.state.path().join("key"),
        other.state.path().join("key"),
    )
    .expect("authorization test fixture operation");
    fs::write(
        other.root.path().join(".zvec-grep/authorization.json"),
        &original,
    )
    .expect("authorization test fixture operation");
    assert!(!other.run(&["auth", "status"]).status.success());
    let mut value: serde_json::Value =
        serde_json::from_slice(&original).expect("authorization test fixture operation");
    value["grant"]["endpoint"] = "https://example.test/embeddings".into();
    fs::write(
        &path,
        serde_json::to_vec(&value).expect("authorization test fixture operation"),
    )
    .expect("authorization test fixture operation");
    assert!(!fixture.run(&["auth", "status"]).status.success());
    fixture.success(&["auth", "revoke"]);
    fixture.success(&["auth", "revoke"]);
    assert!(
        fixture
            .success(&["auth", "status"])
            .contains("not authorized")
    );
}

#[test]
fn indexing_requires_matching_consent_before_credentials_or_network() {
    let fixture = Fixture::new();
    let args = [
        "index",
        "--mode",
        "direct",
        "--embedding",
        "qwen/text-embedding-v4",
    ];
    let denied = fixture.run(&args);
    assert!(String::from_utf8_lossy(&denied.stderr).contains("authorization required"));
    fixture.grant();
    let approved = fixture.run(&args);
    assert!(String::from_utf8_lossy(&approved.stderr).contains("requires an API key"));
    let changed = fixture.run(&[
        "index",
        "--mode",
        "direct",
        "--embedding",
        "qwen/text-embedding-v4",
        "--endpoint",
        "https://example.test/embeddings",
    ]);
    assert!(String::from_utf8_lossy(&changed.stderr).contains("authorization required"));
    fixture.success(&["auth", "revoke"]);
    let once = fixture.run(&[
        "index",
        "--mode",
        "direct",
        "--embedding",
        "qwen/text-embedding-v4",
        "--allow-remote",
    ]);
    assert!(String::from_utf8_lossy(&once.stderr).contains("requires an API key"));
    assert!(
        fixture
            .success(&["auth", "status"])
            .contains("not authorized")
    );
    assert!(String::from_utf8_lossy(&fixture.run(&args).stderr).contains("authorization required"));
}

#[test]
fn rejects_unsupported_scope_capability_and_local_models() {
    let fixture = Fixture::new();
    for args in [
        vec![
            "auth",
            "grant",
            "--capability",
            "embedding",
            "--scope",
            "once",
        ],
        vec![
            "auth",
            "grant",
            "--capability",
            "other",
            "--scope",
            "workspace",
        ],
        vec![
            "auth",
            "grant",
            "--capability",
            "embedding",
            "--scope",
            "workspace",
            "--embedding",
            "local/potion-code-16m-v2",
        ],
    ] {
        assert!(!fixture.run(&args).status.success());
    }
    assert!(!fixture.state.path().join("key").exists());
}

#[test]
fn resident_server_observes_grant_and_revoke_without_restart() {
    let fixture = Fixture::new();
    let socket =
        std::net::TcpListener::bind("127.0.0.1:0").expect("authorization test fixture operation");
    let address = socket
        .local_addr()
        .expect("authorization test fixture operation")
        .to_string();
    drop(socket);
    let _stop = Stop(&fixture);
    fixture.success(&["server", "on", "--listen", &address]);
    let args = [
        "index",
        "--mode",
        "server",
        "--embedding",
        "qwen/text-embedding-v4",
        "--api-key",
        "test-key",
    ];
    assert!(String::from_utf8_lossy(&fixture.run(&args).stderr).contains("authorization required"));
    fixture.grant();
    // An empty workspace initializes the remote runtime without sending content.
    fixture.success(&args);
    let search = [
        "query",
        "--mode",
        "server",
        "--vector",
        "needle",
        "--refresh",
        "off",
    ];
    assert!(String::from_utf8_lossy(&fixture.run(&search).stderr).contains("requires an API key"));
    fixture.success(&["auth", "revoke"]);
    assert!(
        String::from_utf8_lossy(&fixture.run(&search).stderr).contains("authorization required")
    );
    assert!(String::from_utf8_lossy(&fixture.run(&args).stderr).contains("authorization required"));
    fixture.success(&[
        "index",
        "--mode",
        "server",
        "--embedding",
        "qwen/text-embedding-v4",
        "--api-key",
        "test-key",
        "--allow-remote",
    ]);
    assert!(
        fixture
            .success(&["auth", "status"])
            .contains("not authorized")
    );
    assert!(String::from_utf8_lossy(&fixture.run(&args).stderr).contains("authorization required"));
}
