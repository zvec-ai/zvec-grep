use std::{
    fs,
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
    fn run(&self, args: &[&str]) -> Output {
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
        command.args(args).output().expect("run CLI")
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
}

#[test]
fn config_merges_settings_and_index_consumes_model_defaults() {
    let fixture = Fixture::new();
    let output = fixture.success(&[
        "config",
        "provider",
        "set",
        "qwen",
        "--api-key",
        "test-secret",
    ]);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("test-secret"));
    fixture.success(&[
        "config",
        "model",
        "set",
        "local/potion-code-16m-v2",
        "--device",
        "cpu",
        "--default",
    ]);
    fixture.success(&[
        "config",
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
    fixture.success(&["index", "--mode", "direct", "--model-cache", "cache"]);
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.root.path().join(".zvec-grep/manifest.json")).expect("manifest"),
    )
    .expect("JSON");
    assert_eq!(manifest["embedding"]["model"], "potion-code-16m-v2");
    assert_eq!(manifest["embeddingRuntime"]["device"], "cpu");
}

#[test]
fn invalid_config_commands_leave_no_config_file() {
    let fixture = Fixture::new();
    for args in [
        vec![
            "config",
            "provider",
            "set",
            "local",
            "--api-key",
            "test-key",
        ],
        vec![
            "config",
            "model",
            "set",
            "local/potion-code-16m-v2",
            "--endpoint",
            "https://example.test",
        ],
        vec![
            "config",
            "model",
            "set",
            "qwen/text-embedding-v4",
            "--device",
            "cpu",
        ],
        vec!["config", "model", "set", "qwen/text-embedding-v4"],
    ] {
        assert!(!fixture.run(&args).status.success());
    }
    assert!(!fixture.user.path().join(".zvec-grep/config.json").exists());
}

#[test]
fn binary_consumes_human_color_and_debug_options() {
    let fixture = Fixture::new();
    fs::write(fixture.root.path().join("sample.txt"), "needle\n").expect("source");
    let output = fixture.success(&[
        "query", "--rg", "--human", "--color", "always", "--debug", "needle",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Context"));
    assert!(stdout.contains("\x1b["));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Diagnostics:"));
    let output = fixture.success(&["query", "--rg", "--color", "never", "needle"]);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("\x1b["));
}

#[test]
fn explicit_remote_credentials_and_consent_reach_index_execution() {
    let fixture = Fixture::new();
    fixture.success(&[
        "index",
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
        manifest["embeddingRuntime"]["endpoint"],
        "https://example.test/embeddings"
    );
    fixture.success(&[
        "index",
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
        manifest["embeddingRuntime"]["endpoint"],
        "https://example.test/updated"
    );
}
