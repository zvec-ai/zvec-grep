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
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_zg"));
        command
            .current_dir(self.root.path())
            .env("HOME", self.state.path())
            .env("USERPROFILE", self.state.path())
            .env("ZVEC_GREP_HOME", self.state.path())
            .env("USER", "zg-auth-test-user")
            .env("USERNAME", "zg-auth-test-user")
            .env_remove("ZVEC_GREP_EMBEDDING")
            .env_remove("ZVEC_GREP_ENDPOINT")
            .env_remove("ZVEC_GREP_API_KEY")
            .env_remove("DASHSCOPE_API_KEY")
            .env_remove("QWEN_API_KEY")
            .args(args);
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args)
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
            "--auth",
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
        self.0.run(&["--server", "off"]);
    }
}

#[test]
fn grants_are_workspace_local_scoped_and_revocable_without_remote_requests() {
    let fixture = Fixture::new();
    let missing = fixture.success(&["--auth", "status", "--no-color"]);
    assert!(missing.starts_with("○ Remote Embedding is not authorized\n  "));
    assert!(
        missing.contains("\n\nRun\n  zg --auth grant --capability embedding --scope workspace\n")
    );
    assert!(!missing.contains("\x1b["));
    assert!(!fixture.root.path().join(".zvec-grep").exists());
    fixture.grant();
    let status = fixture.success(&["--auth", "status", ".", "--no-color"]);
    assert!(status.starts_with("✓ Remote Embedding is authorized\n  "));
    assert!(status.contains("\n\nAuthorization\n  Scope       Workspace\n"));
    assert!(status.contains("  Target      qwen/text-embedding-v4\n"));
    assert!(status.contains("\n\nStorage\n  Grant       "));
    assert!(!status.contains("https://") && !status.contains("\x1b["));
    assert!(
        !fixture
            .root
            .path()
            .join(".zvec-grep/manifest.json")
            .exists()
    );
    let path = fixture.root.path().join(".zvec-grep/authorization.json");
    let original = fs::read(&path).expect("authorization test fixture operation");
    let styled = fixture.success(&["--auth", "--color", "always", "status"]);
    assert!(styled.contains("\x1b[38;2;111;207;145m✓ Remote Embedding is authorized\x1b[0m"));
    assert!(styled.contains("\n\nAuthorization\n"));
    assert!(!styled.contains("\x1b[1mAuthorization"));
    let plain = fixture.success(&["--auth", "status", "--no-color"]);
    assert!(!plain.contains("\x1b["));
    assert_eq!(fs::read(&path).expect("unchanged grants"), original);
    let other = Fixture::new();
    fs::create_dir_all(other.root.path().join(".zvec-grep"))
        .expect("authorization test fixture operation");
    fs::write(
        other.root.path().join(".zvec-grep/authorization.json"),
        &original,
    )
    .expect("authorization test fixture operation");
    assert!(
        other
            .success(&["--auth", "status"])
            .contains("not authorized")
    );
    let mut value: serde_json::Value =
        serde_json::from_slice(&original).expect("authorization test fixture operation");
    assert_eq!(value[0]["username"], "zg-auth-test-user");
    assert!(
        fs::read_dir(fixture.state.path())
            .expect("user home")
            .next()
            .is_none()
    );
    value[0]["scope"] = "once".into();
    fs::write(
        &path,
        serde_json::to_vec(&value).expect("authorization test fixture operation"),
    )
    .expect("authorization test fixture operation");
    assert!(!fixture.run(&["--auth", "status"]).status.success());
    assert_eq!(
        fixture.success(&["--auth", "revoke"]),
        "Revoked 1 Remote Embedding Workspace grant(s).\n",
    );
    assert_eq!(
        fixture.success(&["--auth", "revoke"]),
        "No Remote Embedding Workspace grants found.\n",
    );
    assert!(
        fixture
            .success(&["--auth", "status"])
            .contains("not authorized")
    );
}

#[test]
fn malformed_grants_can_be_revoked_without_claiming_a_record_count() {
    let fixture = Fixture::new();
    let home = fixture.root.path().join(".zvec-grep");
    fs::create_dir(&home).expect("authorization directory");
    let path = home.join("authorization.json");
    fs::write(&path, "{malformed authorization").expect("malformed grant fixture");
    assert!(!fixture.run(&["--auth", "status"]).status.success());
    assert_eq!(
        fixture.success(&["--auth", "revoke"]),
        "Removed Remote Embedding Workspace authorization.\n",
    );
    assert!(!path.exists());
    assert_eq!(
        fixture.success(&["--auth", "revoke"]),
        "No Remote Embedding Workspace grants found.\n",
    );
}

#[test]
fn remembered_model_grants_coexist_and_remain_scoped_to_each_destination() {
    let fixture = Fixture::new();
    fixture.grant();
    fixture.success(&[
        "--auth",
        "grant",
        ".",
        "--capability",
        "embedding",
        "--scope",
        "workspace",
        "--embedding",
        "qwen/qwen3-vl-embedding",
    ]);
    fixture.grant();
    let path = fixture.root.path().join(".zvec-grep/authorization.json");
    let grants: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("grants")).expect("grant list");
    assert_eq!(grants.as_array().expect("two destinations").len(), 2);
    for model in ["qwen/text-embedding-v4", "qwen/qwen3-vl-embedding"] {
        let status = fixture.success(&["--auth", "status"]);
        assert!(status.contains(model));
        let output = fixture.run(&["--index", "--mode", "direct", "--embedding", model]);
        assert!(String::from_utf8_lossy(&output.stderr).contains("requires an API key"));
    }
    assert_eq!(
        fixture.success(&["--auth", "revoke"]),
        "Revoked 2 Remote Embedding Workspace grant(s).\n",
    );
    assert!(
        fixture
            .success(&["--auth", "status"])
            .contains("not authorized")
    );
}

#[test]
fn indexing_requires_matching_consent_before_credentials_or_network() {
    let fixture = Fixture::new();
    let args = [
        "--index",
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
        "--index",
        "--mode",
        "direct",
        "--embedding",
        "qwen/text-embedding-v4",
        "--endpoint",
        "https://example.test/embeddings",
    ]);
    assert!(String::from_utf8_lossy(&changed.stderr).contains("authorization required"));
    fixture.success(&["--auth", "revoke"]);
    let once = fixture.run(&[
        "--index",
        "--mode",
        "direct",
        "--embedding",
        "qwen/text-embedding-v4",
        "--allow-remote",
    ]);
    assert!(String::from_utf8_lossy(&once.stderr).contains("requires an API key"));
    assert!(
        fixture
            .success(&["--auth", "status"])
            .contains("not authorized")
    );
    assert!(String::from_utf8_lossy(&fixture.run(&args).stderr).contains("authorization required"));
}

#[test]
fn rejects_unsupported_scope_capability_and_local_models() {
    let fixture = Fixture::new();
    for args in [
        vec![
            "--auth",
            "grant",
            "--capability",
            "embedding",
            "--scope",
            "once",
        ],
        vec![
            "--auth",
            "grant",
            "--capability",
            "other",
            "--scope",
            "workspace",
        ],
        vec![
            "--auth",
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
    assert!(
        fs::read_dir(fixture.state.path())
            .expect("user home")
            .next()
            .is_none()
    );
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
    fixture.success(&["--server", "on", "--listen", &address]);
    let args = [
        "--index",
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
    let search = ["--mode", "server", "--vector", "needle", "--refresh", "off"];
    fixture.success(&["--index", "--mode", "server"]);
    fixture.success(&["--auth", "revoke"]);
    assert!(
        String::from_utf8_lossy(&fixture.run(&search).stderr).contains("authorization required")
    );
    assert!(String::from_utf8_lossy(&fixture.run(&args).stderr).contains("authorization required"));
    fixture.success(&[
        "--index",
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
            .success(&["--auth", "status"])
            .contains("not authorized")
    );
    assert!(String::from_utf8_lossy(&fixture.run(&args).stderr).contains("authorization required"));
}

#[test]
fn api_base_url_grants_match_resolved_index_endpoints() {
    let model = "qwen/qwen3.7-text-embedding";
    let base = "https://example.test/compatible-mode/v1/";
    let endpoint = "https://example.test/compatible-mode/v1/embeddings";
    for mode in ["direct", "server"] {
        let fixture = Fixture::new();
        let _stop = Stop(&fixture);
        if mode == "server" {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("available port");
            let address = listener.local_addr().expect("address").to_string();
            drop(listener);
            fixture.success(&["--server", "on", "--listen", &address]);
        }
        fixture.success(&[
            "--auth",
            "grant",
            ".",
            "--capability",
            "embedding",
            "--scope",
            "workspace",
            "--embedding",
            model,
            "--endpoint",
            base,
        ]);
        let grant: serde_json::Value = serde_json::from_slice(
            &fs::read(fixture.root.path().join(".zvec-grep/authorization.json"))
                .expect("authorization"),
        )
        .expect("grant list");
        assert_eq!(grant[0]["endpoint"], endpoint);

        // Empty workspaces resolve and persist the runtime without network I/O.
        // Both spellings must reuse the grant for the actual request URL.
        for requested in [base, endpoint] {
            fixture.success(&[
                "--index",
                "--mode",
                mode,
                "--embedding",
                model,
                "--endpoint",
                requested,
                "--api-key",
                "test-key",
            ]);
            let manifest: serde_json::Value = serde_json::from_slice(
                &fs::read(fixture.root.path().join(".zvec-grep/manifest.json")).expect("manifest"),
            )
            .expect("workspace manifest");
            assert_eq!(
                manifest["index"]["descriptor"]["embeddingConfigs"][model]["endpoint"],
                endpoint
            );
        }
    }
}

#[test]
fn changed_user_requires_consent_without_blocking_reauthorization() {
    let fixture = Fixture::new();
    fixture.grant();
    let grant_path = fixture.root.path().join(".zvec-grep/authorization.json");
    let original = fs::read(&grant_path).expect("original grant");
    let status = fixture
        .command(&["--auth", "status"])
        .env("USER", "another-user")
        .env("USERNAME", "another-user")
        .output()
        .expect("status for another user");
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("not authorized"));
    assert_eq!(fs::read(&grant_path).expect("unchanged grant"), original);

    let granted = fixture
        .command(&[
            "--auth",
            "grant",
            ".",
            "--capability",
            "embedding",
            "--scope",
            "workspace",
            "--embedding",
            "qwen/text-embedding-v4",
        ])
        .env("USER", "another-user")
        .env("USERNAME", "another-user")
        .output()
        .expect("grant for another user");
    assert!(
        granted.status.success(),
        "{}",
        String::from_utf8_lossy(&granted.stderr)
    );
    let status = fixture
        .command(&["--auth", "status"])
        .env("USER", "another-user")
        .env("USERNAME", "another-user")
        .output()
        .expect("status after reauthorization");
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("is authorized"));
    assert!(
        fixture
            .success(&["--auth", "status"])
            .contains("not authorized")
    );
}

#[test]
fn matching_path_and_username_reuse_grants_without_user_home_state() {
    let fixture = Fixture::new();
    fixture.grant();
    let other_home = tempfile::tempdir().expect("another user home");
    let grant_path = fixture.root.path().join(".zvec-grep/authorization.json");
    let original = fs::read(&grant_path).expect("original grant");
    let status = fixture
        .command(&["--auth", "status"])
        .env("HOME", other_home.path())
        .env("USERPROFILE", other_home.path())
        .env("ZVEC_GREP_HOME", other_home.path())
        .output()
        .expect("status in another home");
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("is authorized"));
    assert_eq!(fs::read(&grant_path).expect("unchanged grant"), original);
    for home in [fixture.state.path(), other_home.path()] {
        assert!(fs::read_dir(home).expect("user home").next().is_none());
    }
}

#[test]
fn missing_username_requires_explicit_single_request_authorization() {
    let fixture = Fixture::new();
    fixture.grant();
    let grant_path = fixture.root.path().join(".zvec-grep/authorization.json");
    let original = fs::read(&grant_path).expect("original grant");
    let status = fixture
        .command(&["--auth", "status"])
        .env_remove("USER")
        .env_remove("USERNAME")
        .output()
        .expect("status without username");
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("not authorized"));
    let denied = fixture
        .command(&[
            "--auth",
            "grant",
            ".",
            "--capability",
            "embedding",
            "--scope",
            "workspace",
            "--embedding",
            "qwen/text-embedding-v4",
        ])
        .env_remove("USER")
        .env_remove("USERNAME")
        .output()
        .expect("grant without username");
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--allow-remote"));
    let once = fixture
        .command(&[
            "--index",
            "--mode",
            "direct",
            "--embedding",
            "qwen/text-embedding-v4",
            "--allow-remote",
        ])
        .env_remove("USER")
        .env_remove("USERNAME")
        .output()
        .expect("single request without username");
    assert!(String::from_utf8_lossy(&once.stderr).contains("requires an API key"));
    assert_eq!(fs::read(&grant_path).expect("unchanged grant"), original);
}
