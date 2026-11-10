use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn hash(bytes: &[u8]) -> String {
    let mut output = String::new();
    for byte in Sha256::digest(bytes) {
        write!(output, "{byte:02x}").expect("format digest");
    }
    output
}

fn copy_libraries(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("library directory");
    for entry in fs::read_dir(source).expect("build output") {
        let entry = entry.expect("entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_file()
            && (name.contains(".so")
                || entry.path().extension().is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("dylib") || extension.eq_ignore_ascii_case("dll")
                }))
        {
            fs::copy(entry.path(), destination.join(name)).expect("runtime library");
        }
    }
}

struct Fixture {
    root: TempDir,
    bundle: PathBuf,
    install: PathBuf,
    home: PathBuf,
    token: Option<String>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("fixture");
        let bundle = root.path().join("bundle");
        let home = root.path().join("用户 data");
        let install = home.join("dist");
        fs::create_dir_all(bundle.join("bin")).expect("bin");
        let executable = Path::new(env!("CARGO_BIN_EXE_zg"));
        fs::copy(executable, executable_path(&bundle)).expect("binary");
        #[cfg(windows)]
        fs::copy(
            env!("CARGO_BIN_EXE_zg-launcher"),
            bundle.join("bin/zg-launcher.exe"),
        )
        .expect("native launcher");
        let profile = executable.parent().expect("profile");
        copy_libraries(profile, &bundle.join("lib"));
        if profile.join("lib").is_dir() {
            copy_libraries(&profile.join("lib"), &bundle.join("lib"));
        }
        let fixture = Self {
            root,
            bundle,
            install,
            home,
            token: None,
        };
        fixture.write_manifest("first");
        fixture
    }

    fn write_manifest(&self, marker: &str) {
        self.write_variant(marker, "cpu");
    }

    fn write_variant(&self, marker: &str, variant: &str) {
        fs::write(self.bundle.join("marker"), marker).expect("marker");
        let mut files = BTreeMap::new();
        for directory in [self.bundle.join("bin"), self.bundle.join("lib")] {
            for entry in fs::read_dir(directory).expect("files") {
                let path = entry.expect("file").path();
                let content = fs::read(&path).expect("payload");
                files.insert(
                    path.strip_prefix(&self.bundle)
                        .expect("relative")
                        .to_string_lossy()
                        .replace('\\', "/"),
                    json!({ "bytes": content.len(), "sha256": hash(&content) }),
                );
            }
        }
        files.insert(
            "marker".to_owned(),
            json!({ "bytes": marker.len(), "sha256": hash(marker.as_bytes()) }),
        );
        let os = if cfg!(windows) {
            "win32"
        } else if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        };
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x64"
        };
        let abi = if cfg!(windows) {
            "-msvc"
        } else if cfg!(target_os = "linux") {
            "-gnu"
        } else {
            ""
        };
        fs::write(self.bundle.join("install-manifest.json"), serde_json::to_vec(&json!({
            "schemaVersion": 1, "version": env!("CARGO_PKG_VERSION"), "target": format!("{os}-{arch}{abi}"),
            "accelerator": variant, "files": files
        })).expect("manifest")).expect("write manifest");
    }

    fn command(&self, executable: &Path) -> Command {
        let mut command = Command::new(executable);
        for name in [
            "CODEX_HOME",
            "GROK_HOME",
            "CLAUDE_CONFIG_DIR",
            "QWEN_HOME",
            "QODER_CONFIG_DIR",
            "QODER_IDE_MCP_PATH",
            "COPILOT_HOME",
            "CURSOR_CONFIG_DIR",
            "OPENCODE_CONFIG",
            "PI_CODING_AGENT_DIR",
            "VSCODE_PORTABLE",
            "ZVEC_GREP_INSTALL_SKIP_SERVER",
        ] {
            command.env_remove(name);
        }
        command
            .env("ZVEC_GREP_HOME", &self.home)
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join(".config"))
            .env("APPDATA", self.root.path().join("AppData/Roaming"))
            .env("VSCODE_USER_DIR", self.root.path().join("vscode/User"))
            .env_remove("ZVEC_GREP_SERVER_TOKEN")
            .env_remove("ZVEC_GREP_SERVER_TOKEN_FILE");
        if let Some(token) = &self.token {
            command.env("ZVEC_GREP_SERVER_TOKEN", token);
        }
        command
    }

    fn install_command(&self) -> Command {
        let mut command = self.install_with_agents_command();
        command.arg("--skip-agent-upgrade");
        command
    }

    fn install_with_agents_command(&self) -> Command {
        let mut command = self.command(&executable_path(&self.bundle));
        command
            .args(["--standalone", "--install-dir"])
            .arg(&self.install);
        #[cfg(unix)]
        command.arg("--launcher").arg(self.launcher());
        command
            .args([
                "--accelerator",
                self.manifest()["accelerator"].as_str().expect("variant"),
                "--no-modify-path",
            ])
            .arg("--home")
            .arg(&self.home);
        command
    }

    fn install(&self) -> Output {
        self.install_command().output().expect("install")
    }

    fn manifest(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.bundle.join("install-manifest.json")).expect("manifest"),
        )
        .expect("JSON")
    }

    fn version_dir(&self, variant: &str) -> PathBuf {
        self.install.join(env!("CARGO_PKG_VERSION")).join(format!(
            "{}-{variant}",
            self.manifest()["target"]
                .as_str()
                .expect("fixture operation")
        ))
    }

    fn installed(&self) -> PathBuf {
        let receipt = fs::read(self.install.join("install-receipt.json"))
            .ok()
            .map(|bytes| serde_json::from_slice::<Value>(&bytes).expect("fixture operation"));
        executable_path(
            &receipt
                .and_then(|receipt| receipt["bundle"].as_str().map(|key| self.install.join(key)))
                .unwrap_or_else(|| self.version_dir("cpu")),
        )
    }

    fn activate(&self, variant: &str) -> Output {
        self.command(&executable_path(&self.bundle))
            .args([
                "--standalone",
                "--activate",
                env!("CARGO_PKG_VERSION"),
                "--accelerator",
                variant,
                "--skip-agent-upgrade",
                "--install-dir",
            ])
            .arg(&self.install)
            .output()
            .expect("activate retained bundle")
    }

    fn interrupt(&self, restart: &Value, committed: bool) {
        let receipt: Value = serde_json::from_slice(
            &fs::read(self.install.join("install-receipt.json")).expect("fixture operation"),
        )
        .expect("fixture operation");
        let bundles: Vec<_> = ["cpu", "cuda"]
            .into_iter()
            .filter(|variant| self.version_dir(variant).exists())
            .map(|variant| {
                format!(
                    "{}/{}-{variant}",
                    env!("CARGO_PKG_VERSION"),
                    self.manifest()["target"]
                        .as_str()
                        .expect("fixture operation")
                )
            })
            .collect();
        let journal = json!({ "before": receipt, "after": null, "home": self.home.canonicalize().expect("fixture operation"),
            "restart": restart, "entry": { "path": self.normalized_launcher(), "target": executable_path(&self.install.canonicalize().expect("fixture operation").join("current")),
                "shim_sha256": receipt["launcherSha256"], "existed": true }, "bundles": bundles, "committed": committed });
        fs::write(
            self.install.join("activation.json"),
            serde_json::to_vec(&journal).expect("fixture operation"),
        )
        .expect("fixture operation");
        fs::remove_file(self.install.join("current")).expect("fixture operation");
    }

    fn launcher(&self) -> PathBuf {
        if cfg!(unix) {
            self.root.path().join(".local/bin/zg")
        } else {
            executable_path(&self.install)
        }
    }

    fn normalized_launcher(&self) -> PathBuf {
        let launcher = self.launcher();
        // Resolve directory aliases without following the stable command symlink itself.
        launcher
            .parent()
            .expect("launcher directory")
            .canonicalize()
            .expect("canonical launcher directory")
            .join(launcher.file_name().expect("launcher filename"))
    }

    fn instance(&self) -> Value {
        serde_json::from_slice(&fs::read(self.home.join("daemon/instance.lock")).expect("instance"))
            .expect("record")
    }

    fn pid(&self) -> u64 {
        self.instance()["pid"].as_u64().expect("PID")
    }

    fn instance_token(&self) -> String {
        self.instance()["instanceToken"]
            .as_str()
            .expect("instance token")
            .to_owned()
    }

    fn agent_config(&self) -> PathBuf {
        self.root.path().join(".cursor").join("mcp.json")
    }

    fn write_agent_config(&self) -> Value {
        let config = json!({"mcpServers": {
            "zvec_grep": {
                "command": "node", "args": [self.root.path().join("node_modules/@zvec/zvec-grep/dist/cli/index.js"), "server", "--stdio"],
                "env": {"ZVEC_GREP_HOME": self.home, "CUSTOM": "preserved"}, "timeout": 913
            },
            "native": {"command": executable_path(&self.root.path().join("old install")), "args": ["--server", "--stdio", "--home", self.home]},
            "http": {"url": "http://127.0.0.1:9123/mcp", "headers": {"Authorization": "Bearer keep"}},
            "unrelated": {"command": "other", "args": []}
        }});
        fs::create_dir_all(self.agent_config().parent().expect("config directory"))
            .expect("config directory");
        fs::write(
            self.agent_config(),
            serde_json::to_vec_pretty(&config).expect("JSON"),
        )
        .expect("agent config");
        config
    }

    fn agent_backups(&self) -> Vec<PathBuf> {
        fs::read_dir(self.agent_config().parent().expect("config directory"))
            .expect("config directory")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| path.to_string_lossy().contains(".zg-upgrade-"))
            .collect()
    }

    fn assert_agent_migrated(&self, mut original: Value) {
        let launcher = self.normalized_launcher();
        original["mcpServers"]["zvec_grep"]["command"] = json!(launcher);
        original["mcpServers"]["native"]["command"] = json!(launcher);
        original["mcpServers"]["zvec_grep"]["args"] = json!(["--server", "--stdio"]);
        let updated: Value =
            serde_json::from_slice(&fs::read(self.agent_config()).expect("config"))
                .expect("updated JSON");
        assert_eq!(updated, original);
        assert_eq!(self.agent_backups().len(), 1);
    }

    fn assert_agent_unchanged(&self, original: &[u8]) {
        assert_eq!(fs::read(self.agent_config()).expect("config"), original);
        assert!(self.agent_backups().is_empty());
    }

    fn assert_external_preview(&self, output: Output) {
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        success(output);
        for message in [
            "External daemon is still running. Runtime switch is pending.",
            "PREVIEW ONLY — no agent configuration files were changed.",
            "2 entries need migration",
            "Close all agents/MCP clients using zg",
        ] {
            assert!(stdout.contains(message), "{stdout}");
        }
        let stop = stdout
            .lines()
            .find(|line| line.contains("--server off"))
            .expect("manual stop command");
        assert!(
            stop.contains(self.normalized_launcher().to_string_lossy().as_ref()),
            "{stop}"
        );
        assert!(!stdout.contains("  compatible:"), "{stdout}");
        assert!(!stdout.contains("  would upgrade:"), "{stdout}");
        assert!(
            !stdout.contains(self.agent_config().to_string_lossy().as_ref()),
            "{stdout}"
        );
        assert!(
            stdout.find("--server off").expect("stop command")
                < stdout
                    .find("Reopen your agents")
                    .expect("reopen instruction")
        );
        assert!(
            stdout.contains(
                self.home
                    .canonicalize()
                    .expect("canonical home")
                    .to_string_lossy()
                    .as_ref()
            ),
            "{stdout}"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self
            .command(Path::new(env!("CARGO_BIN_EXE_zg")))
            .args(["--server", "off", "--home"])
            .arg(&self.home)
            .output();
    }
}

fn executable_path(root: &Path) -> PathBuf {
    root.join("bin")
        .join(if cfg!(windows) { "zg.exe" } else { "zg" })
}

#[track_caller]
fn success(output: Output) {
    let Output {
        status,
        stdout,
        stderr,
    } = output;
    assert!(
        status.success(),
        "status: {status}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
}

#[cfg(unix)]
fn conflicting_launcher_keeps_the_daemon(fixture: &Fixture, pid: u64) {
    fs::remove_file(fixture.launcher()).expect("replace command link");
    fs::write(fixture.launcher(), "foreign program").expect("foreign entry");
    let output = fixture.install();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("another installation"));
    assert_eq!(fixture.pid(), pid);
    assert_eq!(
        fs::read_to_string(fixture.launcher()).expect("preserved entry"),
        "foreign program"
    );
    fs::remove_file(fixture.launcher()).expect("remove fixture");
    std::os::unix::fs::symlink(
        executable_path(
            &fixture
                .install
                .canonicalize()
                .expect("root")
                .join("current"),
        ),
        fixture.launcher(),
    )
    .expect("restore command link");
}

#[test]
fn standalone_switches_immutable_bundles_and_restarts_owned_daemon() {
    let fixture = Fixture::new();
    success(fixture.install());
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(fixture.launcher()).expect("fixture operation"),
        executable_path(
            &fixture
                .install
                .canonicalize()
                .expect("fixture operation")
                .join("current")
        )
    );
    let original_binary = fixture
        .installed()
        .canonicalize()
        .expect("fixture operation");
    fs::write(fixture.home.join("user-data"), "keep").expect("fixture operation");
    let socket = TcpListener::bind("127.0.0.1:0").expect("fixture operation");
    let address = socket.local_addr().expect("fixture operation").to_string();
    drop(socket);
    success(
        fixture
            .command(&fixture.launcher())
            .args(["--server", "on", "--listen", &address, "--home"])
            .arg(&fixture.home)
            .output()
            .expect("fixture operation"),
    );
    let first_instance = fixture.instance_token();
    let original_config = fixture.write_agent_config();
    fixture.write_variant("second", "cuda");
    success(
        fixture
            .install_with_agents_command()
            .output()
            .expect("fixture operation"),
    );
    fixture.assert_agent_migrated(original_config);
    // Windows may reuse a PID immediately; the instance token identifies each daemon lifetime.
    assert_ne!(fixture.instance_token(), first_instance);
    assert_eq!(
        fs::read_to_string(fixture.version_dir("cpu").join("marker")).expect("fixture operation"),
        "first"
    );
    assert_eq!(
        fs::read_to_string(fixture.version_dir("cuda").join("marker")).expect("fixture operation"),
        "second"
    );
    assert_ne!(
        fixture
            .installed()
            .canonicalize()
            .expect("fixture operation"),
        original_binary
    );
    let upgraded_instance = fixture.instance_token();
    success(fixture.activate("cpu"));
    assert_eq!(
        fixture
            .installed()
            .canonicalize()
            .expect("fixture operation"),
        original_binary
    );
    assert_ne!(fixture.instance_token(), upgraded_instance);
    assert!(fixture.version_dir("cuda").exists());
    #[cfg(unix)]
    conflicting_launcher_keeps_the_daemon(&fixture, fixture.pid());
    fixture.write_variant("second", "cuda");
    success(
        fixture
            .command(&fixture.launcher())
            .args(["--server", "off", "--home"])
            .arg(&fixture.home)
            .output()
            .expect("fixture operation"),
    );
    fixture.interrupt(&json!({ "home": fixture.home, "listen": address, "toolset": "agent", "tokenFile": null, "authenticationRequired": false }), false);
    success(fixture.install());
    assert!(!fixture.install.join("activation.json").exists());
    assert!(fixture.pid() > 0);
    success(
        fixture
            .install_command()
            .arg("--remove")
            .output()
            .expect("fixture operation"),
    );
    for path in [
        fixture.version_dir("cpu"),
        fixture.version_dir("cuda"),
        fixture.install.join("current"),
        fixture.launcher(),
    ] {
        assert!(fs::symlink_metadata(path).is_err());
    }
    assert_eq!(
        fs::read_to_string(fixture.home.join("user-data")).expect("fixture operation"),
        "keep"
    );
}

#[test]
fn tampered_download_keeps_the_selected_package() {
    let fixture = Fixture::new();
    success(fixture.install());
    let selected = fixture.installed();
    fs::write(fixture.bundle.join("marker"), "corrupt").expect("tamper download");
    let output = fixture.install();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("checksum mismatch"));
    assert_eq!(fixture.installed(), selected);
    assert_eq!(
        fs::read_to_string(fixture.version_dir("cpu").join("marker")).expect("original package"),
        "first"
    );
}

#[test]
fn reinstalling_a_different_payload_cannot_overwrite_a_version_directory() {
    let fixture = Fixture::new();
    success(fixture.install());
    fixture.write_manifest("changed");
    let output = fixture.install();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("immutable version directories"));
    assert_eq!(
        fs::read_to_string(fixture.version_dir("cpu").join("marker")).expect("fixture operation"),
        "first"
    );
    assert!(!fixture.install.join(".staging").exists());
}

#[test]
fn interrupted_authenticated_installation_requires_credentials_before_restarting() {
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;
    use std::time::Duration;

    let mut fixture = Fixture::new();
    success(fixture.install());
    fixture.token = Some("standalone-regression-token-at-least-32-characters".to_owned());
    let listener = TcpListener::bind("127.0.0.1:0").expect("port");
    let address = listener.local_addr().expect("address").to_string();
    drop(listener);
    let anonymous_status = || {
        let mut stream = TcpStream::connect(&address).expect("daemon connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        write!(stream, "POST /mcp HTTP/1.1\r\nHost: {address}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").expect("request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("response");
        response.lines().next().expect("status").to_owned()
    };
    success(
        fixture
            .command(&fixture.installed())
            .args(["--server", "on", "--listen", &address, "--home"])
            .arg(&fixture.home)
            .output()
            .expect("start"),
    );
    assert!(anonymous_status().contains("401"));
    // Missing credentials must fail before creating a journal or stopping the service.
    let denied = fixture
        .install_command()
        .env_remove("ZVEC_GREP_SERVER_TOKEN")
        .output()
        .expect("missing credentials");
    assert!(!denied.status.success());
    assert!(!fixture.install.join("activation.json").exists());
    assert!(anonymous_status().contains("401"));
    success(
        fixture
            .command(&fixture.installed())
            .args(["--server", "off", "--home"])
            .arg(&fixture.home)
            .output()
            .expect("stop"),
    );
    fixture.interrupt(&json!({ "home": fixture.home, "listen": address, "toolset": "agent", "tokenFile": null, "authenticationRequired": true }), false);
    let denied = fixture
        .install_command()
        .env_remove("ZVEC_GREP_SERVER_TOKEN")
        .output()
        .expect("recovery without token");
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr)
            .contains("recovery requires the daemon authentication token")
    );
    assert!(TcpStream::connect(&address).is_err());
    assert!(fixture.install.join("activation.json").exists());
    assert!(fixture.version_dir("cpu").exists());
    success(fixture.install());
    assert!(anonymous_status().contains("401"));
    assert!(!fixture.install.join("activation.json").exists());
}

#[test]
fn interrupted_installation_reuses_saved_token_file_with_running_daemon() {
    recover_with_saved_token_file(false);
}

#[test]
fn interrupted_installation_reuses_saved_token_file_after_daemon_stopped() {
    recover_with_saved_token_file(true);
}

fn recover_with_saved_token_file(stop_before_recovery: bool) {
    let mut fixture = Fixture::new();
    success(fixture.install());
    let token = "saved-file-regression-token-at-least-32-characters";
    let token_file = fixture.root.path().join("daemon token.txt");
    fs::write(&token_file, token).expect("token file");
    let listener = TcpListener::bind("127.0.0.1:0").expect("port");
    let address = listener.local_addr().expect("address").to_string();
    drop(listener);
    success(
        fixture
            .command(&fixture.installed())
            .args(["--server", "on", "--listen", &address, "--home"])
            .arg(&fixture.home)
            .arg("--token-file")
            .arg(&token_file)
            .output()
            .expect("start authenticated daemon"),
    );
    // Keep cleanup authenticated, but remove this environment token from every recovery call.
    fixture.token = Some(token.to_owned());
    let original_instance = fixture.instance_token();
    if stop_before_recovery {
        success(
            fixture
                .command(&fixture.installed())
                .args(["--server", "off", "--home"])
                .arg(&fixture.home)
                .output()
                .expect("stop before interruption"),
        );
    }
    fixture.interrupt(
        &json!({ "home": fixture.home, "listen": address, "toolset": "agent",
        "tokenFile": token_file, "authenticationRequired": true }),
        false,
    );
    fixture.write_variant("resumed authenticated upgrade", "cuda");
    let denied = fixture
        .install_command()
        .env_remove("ZVEC_GREP_SERVER_TOKEN")
        .arg("--token-file")
        .arg(fixture.root.path().join("missing override.txt"))
        .output()
        .expect("invalid explicit override");
    assert!(!denied.status.success());
    assert!(fixture.install.join("activation.json").exists());
    assert!(!fixture.install.join("current").exists());
    success(
        fixture
            .install_command()
            .env_remove("ZVEC_GREP_SERVER_TOKEN")
            .output()
            .expect("recover and continue without repeating token-file"),
    );
    assert_ne!(fixture.instance_token(), original_instance);
    assert!(fixture.installed().starts_with(fixture.version_dir("cuda")));
    assert!(!fixture.install.join("activation.json").exists());
    let instance = fixture.instance_token();
    let anonymous = fixture
        .command(&fixture.installed())
        .env_remove("ZVEC_GREP_SERVER_TOKEN")
        .args(["--server", "off", "--home"])
        .arg(&fixture.home)
        .output()
        .expect("anonymous shutdown is rejected");
    assert!(!anonymous.status.success());
    assert_eq!(fixture.instance_token(), instance);
}

#[test]
fn standalone_uninstall_resumes_committed_cleanup() {
    let fixture = Fixture::new();
    success(fixture.install());
    fs::write(fixture.home.join("user-data"), "keep").expect("data");
    fixture.interrupt(&Value::Null, true);
    fs::remove_file(fixture.version_dir("cpu").join("marker")).expect("partial removal");
    success(
        fixture
            .command(&executable_path(&fixture.bundle))
            .args(["--standalone", "--remove", "--install-dir"])
            .arg(&fixture.install)
            .output()
            .expect("recover and remove"),
    );
    for name in [
        "current",
        ".staging",
        "activation.json",
        env!("CARGO_PKG_VERSION"),
    ] {
        assert!(!fixture.install.join(name).exists(), "{name}");
    }
    #[cfg(unix)]
    assert!(fs::symlink_metadata(fixture.launcher()).is_err());
    assert_eq!(
        fs::read_to_string(fixture.home.join("user-data")).expect("preserved data"),
        "keep"
    );
}

#[test]
fn uninstall_cleans_an_interrupted_first_activation() {
    let fixture = Fixture::new();
    success(fixture.install());
    fixture.interrupt(&Value::Null, false);
    let journal_path = fixture.install.join("activation.json");
    let mut journal: Value =
        serde_json::from_slice(&fs::read(&journal_path).expect("journal")).expect("JSON");
    journal["after"] = journal["before"].take();
    journal["entry"]["existed"] = json!(false);
    fs::write(&journal_path, serde_json::to_vec(&journal).expect("JSON")).expect("journal");
    fs::remove_file(fixture.install.join("install-receipt.json")).expect("receipt not committed");
    success(
        fixture
            .install_command()
            .arg("--remove")
            .output()
            .expect("recover first install"),
    );
    assert!(!fixture.version_dir("cpu").exists());
    assert!(fs::symlink_metadata(fixture.launcher()).is_err());
    assert!(!journal_path.exists());
}

#[cfg(unix)]
#[test]
fn standalone_uninstall_preserves_a_replaced_command_entry() {
    let fixture = Fixture::new();
    success(fixture.install());
    fs::remove_file(fixture.launcher()).expect("replace link");
    fs::write(fixture.launcher(), "unrelated executable").expect("replacement");
    success(
        fixture
            .command(&executable_path(&fixture.bundle))
            .args(["--standalone", "--install-dir"])
            .arg(&fixture.install)
            .arg("--remove")
            .output()
            .expect("uninstall"),
    );
    assert_eq!(
        fs::read_to_string(fixture.launcher()).expect("preserved command"),
        "unrelated executable"
    );
    assert!(!fixture.installed().exists());
}

#[test]
fn external_daemon_allows_install_and_recovery_but_only_previews_agent_migration() {
    let mut fixture = Fixture::new();
    fixture.token = Some("external-daemon-regression-token-at-least-32-characters".to_owned());
    let original_config = fixture.write_agent_config();
    let original_bytes = fs::read(fixture.agent_config()).expect("original config");
    let socket = TcpListener::bind("127.0.0.1:0").expect("port");
    let address = socket.local_addr().expect("address").to_string();
    drop(socket);
    let executable = Path::new(env!("CARGO_BIN_EXE_zg"));
    success(
        fixture
            .command(executable)
            .args(["--server", "on", "--listen", &address, "--home"])
            .arg(&fixture.home)
            .output()
            .expect("start external installation"),
    );
    let instance = fixture.instance_token();
    let install_without_credentials = || {
        fixture
            .install_with_agents_command()
            .env_remove("ZVEC_GREP_SERVER_TOKEN")
            .output()
            .expect("install alongside external daemon")
    };
    fixture.assert_external_preview(install_without_credentials());
    assert!(fixture.installed().exists());
    assert_eq!(fixture.instance_token(), instance);
    fixture.assert_agent_unchanged(&original_bytes);

    // Recovery without an owned service must also leave the external daemon untouched.
    fixture.interrupt(&Value::Null, false);
    fixture.write_variant("second", "cuda");
    success(install_without_credentials());
    assert!(!fixture.install.join("activation.json").exists());
    assert_eq!(fixture.instance_token(), instance);
    fixture.assert_agent_unchanged(&original_bytes);
    assert_eq!(
        fs::read_to_string(fixture.version_dir("cuda").join("marker")).expect("current"),
        "second"
    );

    success(
        fixture
            .command(&executable_path(&fixture.bundle))
            .args(["--standalone", "--remove", "--install-dir"])
            .arg(&fixture.install)
            .env_remove("ZVEC_GREP_SERVER_TOKEN")
            .output()
            .expect("uninstall alongside external daemon"),
    );
    assert!(!fixture.installed().exists());
    assert_eq!(fixture.instance_token(), instance);
    fixture.assert_agent_unchanged(&original_bytes);
    success(install_without_credentials());
    success(
        fixture
            .command(&fixture.launcher())
            .args(["--server", "off", "--home"])
            .arg(&fixture.home)
            .output()
            .expect("stop external installation"),
    );
    success(install_without_credentials());
    fixture.assert_agent_migrated(original_config);
    assert!(std::net::TcpStream::connect(address).is_err());
}

#[test]
fn recovery_cannot_replace_an_external_daemon_when_owned_restart_is_pending() {
    let fixture = Fixture::new();
    success(fixture.install());
    let socket = TcpListener::bind("127.0.0.1:0").expect("port");
    let address = socket.local_addr().expect("address").to_string();
    drop(socket);
    fixture.interrupt(&json!({ "home": fixture.home, "listen": address, "toolset": "agent", "tokenFile": null, "authenticationRequired": false }), false);
    success(
        fixture
            .command(Path::new(env!("CARGO_BIN_EXE_zg")))
            .args(["--server", "on", "--listen", &address, "--home"])
            .arg(&fixture.home)
            .output()
            .expect("start external daemon"),
    );
    let instance = fixture.instance_token();
    let output = fixture.install();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("another executable"));
    assert_eq!(fixture.instance_token(), instance);
    assert!(fixture.install.join("activation.json").exists());
    assert!(fixture.version_dir("cpu").exists());
    assert!(!fixture.install.join("current").exists());
}

#[test]
fn daemon_from_a_retained_bundle_is_managed_without_moving_its_files() {
    let fixture = Fixture::new();
    success(fixture.install());
    fixture.write_variant("second", "cuda");
    success(fixture.install());
    success(fixture.activate("cpu"));
    let socket = TcpListener::bind("127.0.0.1:0").expect("fixture operation");
    let address = socket.local_addr().expect("fixture operation").to_string();
    drop(socket);
    let executable = executable_path(&fixture.version_dir("cuda"));
    success(
        fixture
            .command(&executable)
            .args(["--server", "on", "--listen", &address, "--home"])
            .arg(&fixture.home)
            .output()
            .expect("fixture operation"),
    );
    let instance = fixture.instance_token();
    success(fixture.activate("cpu"));
    assert_ne!(fixture.instance_token(), instance);
    assert!(executable.exists());
    assert!(fixture.installed().starts_with(fixture.version_dir("cpu")));
}

#[test]
fn newly_installed_integrations_keep_the_fixed_command_across_version_switches() {
    let fixture = Fixture::new();
    success(fixture.install());
    success(
        fixture
            .command(&fixture.launcher())
            .args([
                "--install",
                "--yes",
                "--target",
                "cursor",
                "--target",
                "opencode",
                "--mcp-transport",
                "stdio",
            ])
            .env("ZVEC_GREP_INSTALL_SKIP_SERVER", "1")
            .output()
            .expect("new integrations"),
    );
    success(
        fixture
            .command(&fixture.launcher())
            .args([
                "--install",
                "--yes",
                "--target",
                "cursor",
                "--target",
                "opencode",
                "--mcp-transport",
                "stdio",
            ])
            .env("ZVEC_GREP_INSTALL_SKIP_SERVER", "1")
            .output()
            .expect("idempotent integration install"),
    );
    let original = fs::read(fixture.agent_config()).expect("new configuration");
    let config: Value = serde_json::from_slice(&original).expect("JSON");
    assert_eq!(
        config["mcpServers"]["zvec_grep"]["command"],
        json!(fixture.normalized_launcher())
    );
    let array_path = fixture.root.path().join(".config/opencode/opencode.json");
    let array_original = fs::read(&array_path).expect("command-array configuration");
    let array_config: Value = serde_json::from_slice(&array_original).expect("JSON");
    assert_eq!(
        array_config["mcp"]["zvec_grep"]["command"][0],
        json!(fixture.normalized_launcher())
    );
    fixture.write_variant("second", "cuda");
    success(fixture.install());
    success(fixture.activate("cpu"));
    assert_eq!(
        fs::read(fixture.agent_config()).expect("fixed configuration"),
        original
    );
    assert_eq!(
        fs::read(array_path).expect("fixed command array"),
        array_original
    );
    success(
        fixture
            .command(&fixture.launcher())
            .args([
                "--uninstall",
                "--yes",
                "--target",
                "cursor",
                "--target",
                "opencode",
            ])
            .output()
            .expect("remove managed integrations"),
    );
}

#[test]
fn agent_migration_failure_does_not_roll_back_a_successful_installation() {
    let fixture = Fixture::new();
    fixture.write_agent_config();
    fs::write(fixture.agent_config(), "{ invalid json").expect("broken config");
    let output = fixture
        .install_with_agents_command()
        .output()
        .expect("install");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    success(output);
    assert!(stdout.contains("migration needs attention"), "{stdout}");
    assert!(
        stdout.contains(fixture.agent_config().to_string_lossy().as_ref()),
        "expected configuration path {:?} in:\n{stdout}",
        fixture.agent_config()
    );
    assert!(fixture.installed().exists());
    assert!(!fixture.install.join("activation.json").exists());
    assert_eq!(
        fs::read_to_string(fixture.agent_config()).expect("config"),
        "{ invalid json"
    );
    assert!(fixture.agent_backups().is_empty());
}

async fn mcp_ping(command: Command) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::{io, process::Stdio, time::Duration};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut child = tokio::process::Command::from(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("the fixed command entry must launch");
    let mut input = child.stdin.take().expect("MCP input");
    let mut lines = BufReader::new(child.stdout.take().expect("MCP output")).lines();
    let exchange = async {
        for request in [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"installer-regression","version":"1"}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"ping"}),
        ] {
            input.write_all(format!("{request}\n").as_bytes()).await?;
            loop {
                let line = lines.next_line().await?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "MCP disconnected during daemon replacement",
                    )
                })?;
                let response: Value = serde_json::from_str(&line)?;
                if response["id"] == request["id"] {
                    if response.get("result").is_none() {
                        return Err(io::Error::other(format!("MCP response: {response}")).into());
                    }
                    break;
                }
            }
            if request["id"] == 1 {
                input
                    .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
                    .await?;
            }
        }
        drop(input);
        if !child.wait().await?.success() {
            return Err(io::Error::other("MCP client exited unsuccessfully").into());
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };
    tokio::time::timeout(Duration::from_secs(30), exchange).await?
}

async fn repeated_version_probe(command: Command) {
    use std::time::Duration;
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    for _ in 0..6 {
        let output = tokio::time::timeout(Duration::from_secs(15), command.output())
            .await
            .expect("CLI deadline")
            .expect("fixed entry must launch");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            env!("CARGO_PKG_VERSION")
        );
        success(output);
    }
}

#[tokio::test]
async fn external_stdio_instructions_migrate_then_stop_before_reopening() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").expect("port");
    let address = listener.local_addr().expect("address").to_string();
    drop(listener);
    let executable = Path::new(env!("CARGO_BIN_EXE_zg"));
    success(
        fixture
            .command(executable)
            .args([
                "--server",
                "on",
                "--listen",
                &address,
                "--mcp-toolset",
                "full",
                "--home",
            ])
            .arg(&fixture.home)
            .output()
            .expect("external daemon"),
    );
    let original_instance = fixture.instance_token();
    let args = json!([
        "server",
        "--stdio",
        "--home",
        fixture.home,
        "--listen",
        address,
        "--mcp-toolset",
        "full"
    ]);
    fixture.write_agent_config();
    let config = json!({"mcpServers":{"zvec_grep":{"command":"zg", "args":args}}});
    let original = serde_json::to_vec(&config).expect("JSON");
    fs::write(fixture.agent_config(), &original).expect("stdio configuration");
    let output = fixture
        .install_with_agents_command()
        .output()
        .expect("install preview");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    success(output);
    assert!(stdout.contains("1 entries need migration"), "{stdout}");
    assert!(!stdout.contains("--server on"), "{stdout}");
    assert_eq!(fixture.instance_token(), original_instance);
    fixture.assert_agent_unchanged(&original);
    let commands = stdout
        .split("Close all agents/MCP clients using zg, then run:\n")
        .nth(1)
        .expect("commands")
        .split("Reopen your agents.")
        .next()
        .expect("commands end");
    let mut shell = fixture.command(Path::new(if cfg!(windows) {
        "powershell.exe"
    } else {
        "sh"
    }));
    if cfg!(windows) {
        shell.args(["-NoProfile", "-Command"]);
    } else {
        shell.arg("-c");
    }
    // A failed migration must not stop the running daemon when the printed commands are pasted.
    fs::write(fixture.agent_config(), "{ invalid JSON").expect("broken configuration");
    let _ = shell
        .arg(commands)
        .output()
        .expect("failed manual migration");
    assert_eq!(fixture.instance_token(), original_instance);
    fs::write(fixture.agent_config(), original).expect("restore configuration");
    success(shell.output().expect("manual switch"));
    let updated: Value =
        serde_json::from_slice(&fs::read(fixture.agent_config()).expect("configuration"))
            .expect("JSON");
    let mut expected_args = args;
    expected_args[0] = json!("--server");
    assert_eq!(updated["mcpServers"]["zvec_grep"]["args"], expected_args);
    assert!(
        std::net::TcpStream::connect(&address).is_err(),
        "stop happens before reconnect"
    );
    let mut bridge = fixture.command(&fixture.launcher());
    bridge.args(
        expected_args
            .as_array()
            .expect("args")
            .iter()
            .map(|value| value.as_str().expect("argument")),
    );
    mcp_ping(bridge).await.expect("reopened stdio client");
    assert_ne!(fixture.instance_token(), original_instance);
    assert_eq!(fixture.instance()["mcpToolset"], "full");
}

#[tokio::test]
async fn one_upgrade_serializes_installers_and_allows_concurrent_cli_and_mcp_clients() {
    use std::{process::Stdio, time::Duration};
    let fixture = Fixture::new();
    success(fixture.install());
    let listener = TcpListener::bind("127.0.0.1:0").expect("port");
    let address = listener.local_addr().expect("address").to_string();
    drop(listener);
    success(
        fixture
            .command(&fixture.launcher())
            .args(["--server", "on", "--listen", &address, "--home"])
            .arg(&fixture.home)
            .output()
            .expect("start owned daemon"),
    );
    let instance = fixture.instance_token();
    let original = fixture.installed();
    // Hold the daemon claim so the first installer demonstrably owns the install lock
    // before another installer and ordinary clients are launched.
    let claim = zg_daemon::InstallationDaemon::acquire(
        &fixture.home,
        std::slice::from_ref(&original),
        None,
    )
    .await
    .expect("hold daemon claim");
    fixture.write_variant("concurrent upgrade", "cuda");
    let mut upgrade = tokio::process::Command::from(fixture.install_command())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("upgrade");
    // Copying and hashing a debug package can exceed 30 seconds on shared CI runners.
    tokio::time::timeout(Duration::from_secs(120), async {
        while !fixture.version_dir("cuda").exists() {
            assert!(
                upgrade.try_wait().expect("upgrade status").is_none(),
                "installer stopped before staging"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("candidate prepared before daemon claim");
    let competing = tokio::process::Command::from(fixture.install_command())
        .output()
        .await
        .expect("competing installer");
    assert!(!competing.status.success());
    assert!(
        String::from_utf8_lossy(&competing.stderr)
            .contains("another standalone installation is running")
    );
    let mut clients = Vec::new();
    for _ in 0..4 {
        let mut command = fixture.command(&fixture.launcher());
        command.arg("--version");
        clients.push(tokio::spawn(repeated_version_probe(command)));
    }
    let mcp_command = || {
        let mut command = fixture.command(&fixture.launcher());
        command
            .args(["--server", "--stdio", "--home"])
            .arg(&fixture.home)
            .args(["--listen", &address]);
        command
    };
    let bridges: Vec<_> = (0..2)
        .map(|_| tokio::spawn(mcp_ping(mcp_command())))
        .collect();
    drop(claim);
    success(
        tokio::time::timeout(Duration::from_secs(40), upgrade.wait_with_output())
            .await
            .expect("upgrade deadline")
            .expect("upgrade output"),
    );
    for client in clients {
        client.await.expect("CLI worker");
    }
    for bridge in bridges {
        // A connection attached to the old daemon may close; reconnect through the same entry.
        if let Err(error) = bridge.await.expect("MCP process launched") {
            let disconnected = error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                )
            });
            assert!(disconnected, "unexpected MCP startup failure: {error}");
        }
        mcp_ping(mcp_command())
            .await
            .expect("MCP reconnects after upgrade");
    }
    assert_ne!(fixture.instance_token(), instance);
    assert!(original.is_file(), "the old package remains available");
    assert!(fixture.installed().starts_with(fixture.version_dir("cuda")));
    assert!(!fixture.install.join("activation.json").exists());
}
