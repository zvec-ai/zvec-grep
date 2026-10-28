use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: TempDir::new().expect("temporary home"),
        }
    }
    fn path(&self, path: &str) -> PathBuf {
        self.root.path().join(path)
    }
    fn write(&self, path: &str, content: &str) -> PathBuf {
        let path = self.path(path);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, content).expect("write fixture");
        path
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_zg"));
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
            "ZVEC_GREP_HOME",
            "ZVEC_GREP_INSTALL_SKIP_SERVER",
        ] {
            command.env_remove(name);
        }
        command
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("XDG_CONFIG_HOME", self.path(".config"))
            .env("APPDATA", self.path("AppData/Roaming"))
            .env("VSCODE_USER_DIR", self.path("vscode/User"))
            .env("ZVEC_GREP_HOME", self.path(".zvec-grep"));
        command
    }
    fn run(&self, dry_run: bool) -> Output {
        let mut command = self.command();
        command.args(["--install", "--upgrade-installed"]);
        if dry_run {
            command.arg("--dry-run");
        }
        command.output().expect("run upgrade")
    }
}

fn success(output: &Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn backups(path: &Path) -> Vec<PathBuf> {
    fs::read_dir(path.parent().expect("parent"))
        .expect("read directory")
        .map(|entry| entry.expect("entry").path())
        .filter(|entry| {
            entry
                .file_name()
                .expect("filename")
                .to_string_lossy()
                .contains(".zg-upgrade-")
        })
        .collect()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read")).expect("JSON")
}

#[test]
fn empty_upgrade_creates_no_configuration_or_daemon() {
    let fixture = Fixture::new();
    assert!(success(&fixture.run(false)).contains("No existing integrations found"));
    assert_eq!(fs::read_dir(fixture.root.path()).expect("home").count(), 0);
}

#[test]
fn upgrade_rejects_installation_options_and_requires_upgrade_for_preview() {
    let fixture = Fixture::new();
    for options in [
        vec!["--target", "all"],
        vec!["all"],
        vec!["--yes"],
        vec!["--force"],
        vec!["--mcp-transport", "http"],
        vec!["--mcp-toolset", "full"],
        vec!["--mcp-tool-timeout", "900"],
        vec!["--mcp-token-env", "TOKEN"],
    ] {
        let output = fixture
            .command()
            .args(["--install", "--upgrade-installed"])
            .args(&options)
            .output()
            .expect("run");
        assert!(!output.status.success(), "{options:?}");
    }
    assert!(
        !fixture
            .command()
            .args(["--install", "--dry-run"])
            .output()
            .expect("run")
            .status
            .success()
    );
    assert_eq!(fs::read_dir(fixture.root.path()).expect("home").count(), 0);
}

#[test]
fn upgrades_existing_entries_without_resetting_settings_or_installing_companions() {
    let fixture = Fixture::new();
    let server = json!({"command":"zg", "args":["server", "--stdio", "--mcp-toolset", "full", "--home", fixture.path("custom home")], "env":{"KEY":"value"}, "cwd":fixture.path("workspace"), "disabled":true, "timeout":913, "custom":{"preserved":true}});
    let source = json!({"mcpServers":{"zvec_grep":server, "other":{"command":"other", "args":["server","--stdio"]}}, "permissions":{"allow":["custom"]}});
    let path = fixture.write(
        ".qoder/settings.json",
        &serde_json::to_string_pretty(&source).expect("serialize"),
    );
    let guidance = fixture.write(".qoder/AGENTS.md", "Existing user instructions\n");
    assert!(success(&fixture.run(false)).contains("Upgraded: 1"));
    let mut expected = source;
    expected["mcpServers"]["zvec_grep"]["args"][0] = json!("--server");
    assert_eq!(read_json(&path), expected);
    assert_eq!(
        fs::read_to_string(guidance).expect("guidance"),
        "Existing user instructions\n"
    );
    assert!(!fixture.path(".qoder/mcp.json").exists());
    assert!(!fixture.path(".zvec-grep").exists());
}

#[test]
fn preview_and_repeated_runs_preserve_comments_and_make_exactly_one_backup() {
    let fixture = Fixture::new();
    let source = "{\n // retain file comment\n \"mcp\": {\"zvec_grep\": {\"type\": \"local\", \"command\": [\"zg\", /* action */ \"server\", \"--stdio\"], \"enabled\": false,},},\n}\n";
    let path = fixture.write(".config/opencode/opencode.jsonc", source);
    assert!(success(&fixture.run(true)).contains("Would upgrade: 1"));
    assert_eq!(fs::read_to_string(&path).expect("preview"), source);
    assert!(backups(&path).is_empty());
    success(&fixture.run(false));
    let expected = source.replace("\"server\"", "\"--server\"");
    assert_eq!(fs::read_to_string(&path).expect("upgraded"), expected);
    let saved = backups(&path);
    assert_eq!(saved.len(), 1);
    assert_eq!(fs::read_to_string(&saved[0]).expect("backup"), source);
    assert!(success(&fixture.run(false)).contains("Upgraded: 0, compatible: 1"));
    assert_eq!(backups(&path), saved);
    assert_eq!(fs::read_to_string(&path).expect("unchanged"), expected);
}

#[test]
fn toml_migration_changes_only_the_action_token() {
    let fixture = Fixture::new();
    let source = "# user settings\n[mcp_servers.other]\ncommand = 'other'\n\n# ZVEC_GREP_START\n[mcp_servers.zvec_grep]\ncommand = 'zg'\nargs = [\n 'server', # keep this comment\n '--stdio', '--mcp-toolset', 'full',\n]\ntool_timeout_sec = 913\nenabled = false\n[mcp_servers.zvec_grep.env]\nKEY = 'user value'\n# ZVEC_GREP_END\n";
    for name in [".codex/config.toml", ".grok/config.toml"] {
        fixture.write(name, source);
    }
    assert!(success(&fixture.run(false)).contains("Upgraded: 2"));
    for name in [".codex/config.toml", ".grok/config.toml"] {
        assert_eq!(
            fs::read_to_string(fixture.path(name)).expect("read"),
            source.replace("'server'", "\"--server\"")
        );
    }
    assert!(!fixture.path(".codex/AGENTS.md").exists());
}

#[test]
fn migrates_known_node_entrypoint_to_the_running_native_executable() {
    for description in ["Managed by zg install", "Managed by zg --install"] {
        let fixture = Fixture::new();
        let node = fixture.path(if cfg!(windows) {
            "node directory/node.exe"
        } else {
            "node directory/node"
        });
        let script = fixture.path("old package/dist/cli/index.js");
        let source = json!({"mcpServers":{"zvec_grep":{"description":description, "command":node, "args":[script,"server","--stdio","--mcp-toolset","full"], "timeout":700_000, "env":{"TOKEN":"keep"}}}});
        let path = fixture.write(
            ".qoder/mcp.json",
            &serde_json::to_string(&source).expect("serialize"),
        );
        success(&fixture.run(false));
        let mut expected = source;
        let actual = read_json(&path);
        let executable = actual["mcpServers"]["zvec_grep"]["command"]
            .as_str()
            .expect("executable");
        assert_eq!(
            fs::canonicalize(executable).expect("native path"),
            fs::canonicalize(env!("CARGO_BIN_EXE_zg")).expect("binary")
        );
        expected["mcpServers"]["zvec_grep"]["command"] = json!(executable);
        expected["mcpServers"]["zvec_grep"]["args"] =
            json!(["--server", "--stdio", "--mcp-toolset", "full"]);
        assert_eq!(read_json(&path), expected);
        assert!(!fixture.path(".qoder/settings.json").exists());
        success(&fixture.run(false));
        assert_eq!(backups(&path).len(), 1);
    }
}

#[test]
fn covers_json_locations_and_preserves_http_entries_byte_for_byte() {
    let fixture = Fixture::new();
    for (name, container) in [
        (".claude.json", "mcpServers"),
        (".cursor/mcp.json", "mcpServers"),
        (".qwen/settings.json", "mcpServers"),
        (".copilot/mcp-config.json", "mcpServers"),
        ("vscode/User/mcp.json", "servers"),
        (".pi/agent/mcp.json", "mcpServers"),
    ] {
        fixture.write(
            name,
            &json!({container:{"zvec_grep":{"command":"zg", "args":["server","--stdio"]}}})
                .to_string(),
        );
    }
    let http = "{\"mcpServers\": {\"zvec_grep\": {\"httpUrl\": \"https://custom.example/mcp\", \"headers\": {\"Authorization\": \"${TOKEN}\"}, \"enabled\": false}}}\n";
    let path = fixture.write(".qoder/mcp.json", http);
    assert!(success(&fixture.run(false)).contains("Upgraded: 6, compatible: 1"));
    assert_eq!(fs::read_to_string(&path).expect("http"), http);
    assert!(backups(&path).is_empty());
    assert!(!fixture.path(".claude/settings.json").exists());
    assert!(!fixture.path(".copilot/instructions").exists());
}

#[test]
fn invalid_and_unsupported_configs_do_not_block_independent_upgrades() {
    let fixture = Fixture::new();
    let broken = fixture.write(".claude.json", "{broken");
    let unsupported = "{\"mcpServers\": {\"zvec_grep\": {\"command\": \"node\", \"args\": [\"custom.js\", \"server\", \"--stdio\"]}}}";
    let manual = fixture.write(".qoder/mcp.json", unsupported);
    let path = fixture.write(
        ".cursor/mcp.json",
        "{\"mcpServers\":{\"zvec_grep\":{\"command\":\"zg\",\"args\":[\"server\",\"--stdio\"]}}}",
    );
    let output = fixture.run(false);
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Upgraded: 1, compatible: 0, manual: 1, failed: 1"),
        "{stdout}"
    );
    assert_eq!(fs::read_to_string(broken).expect("broken"), "{broken");
    assert_eq!(fs::read_to_string(&manual).expect("manual"), unsupported);
    assert!(backups(&manual).is_empty());
    assert_eq!(
        read_json(&path)["mcpServers"]["zvec_grep"]["args"][0],
        "--server"
    );
}

#[test]
fn respects_explicit_locations_and_does_not_migrate_background_start_as_stdio() {
    let fixture = Fixture::new();
    let default = fixture.write(
        ".cursor/mcp.json",
        "{\"mcpServers\":{\"zvec_grep\":{\"command\":\"zg\",\"args\":[\"server\",\"on\"]}}}",
    );
    let custom = fixture.write(
        "custom/mcp.json",
        "{\"mcpServers\":{\"zvec_grep\":{\"command\":\"zg\",\"args\":[\"server\",\"--stdio\"]}}}",
    );
    success(
        &fixture
            .command()
            .args(["--install", "--upgrade-installed"])
            .env("CURSOR_CONFIG_DIR", fixture.path("custom"))
            .output()
            .expect("run"),
    );
    assert_eq!(
        read_json(&custom)["mcpServers"]["zvec_grep"]["args"][0],
        "--server"
    );
    assert_eq!(
        read_json(&default)["mcpServers"]["zvec_grep"]["args"][0],
        "server"
    );
    assert!(!fixture.run(false).status.success());
    assert!(backups(&default).is_empty());
}

#[cfg(unix)]
#[test]
fn preserves_symlinks_file_permissions_and_backups_at_the_target() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    let fixture = Fixture::new();
    let source =
        "{\"mcpServers\":{\"zvec_grep\":{\"command\":\"zg\",\"args\":[\"server\",\"--stdio\"]}}}";
    let target = fixture.write("shared/settings.json", source);
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("mode");
    fs::create_dir_all(fixture.path(".qoder")).expect("directory");
    symlink(&target, fixture.path(".qoder/settings.json")).expect("symlink");
    success(&fixture.run(false));
    assert!(
        fs::symlink_metadata(fixture.path(".qoder/settings.json"))
            .expect("link")
            .is_symlink()
    );
    assert_eq!(
        fs::metadata(&target).expect("mode").permissions().mode() & 0o777,
        0o600
    );
    let backup = backups(&target).pop().expect("backup");
    assert_eq!(
        fs::metadata(&backup)
            .expect("backup mode")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(fs::read_to_string(backup).expect("backup content"), source);
}

#[test]
fn migrates_windows_node_paths_and_keeps_argument_comments() {
    let fixture = Fixture::new();
    let source = r#"{
  "mcpServers": {
    "zvec_grep": {
      "description": "Managed by zg install",
      "command": "C:\\Program Files\\nodejs\\node.exe",
      "args": [
        "C:\\Users\\user\\node_modules\\@zvec\\zvec-grep\\dist\\cli\\index.js", // old entry
        "--server", /* keep protocol */ "--stdio"
      ],
      "env": {"CUSTOM": "preserved"}
    }
  }
}"#;
    let path = fixture.write(".qoder/mcp.json", source);
    success(&fixture.run(false));
    let updated = fs::read_to_string(path).expect("updated");
    assert!(updated.contains("// old entry"));
    assert!(updated.contains("/* keep protocol */"));
    assert!(!updated.contains("index.js"));
    assert!(updated.contains("\"CUSTOM\": \"preserved\""));
}

#[test]
fn migrates_toml_node_launchers_without_removing_comments() {
    let fixture = Fixture::new();
    let source = "[mcp_servers.zvec_grep]\ncommand = 'node'\nargs = [\n '/some/node_modules/@zvec/zvec-grep/dist/cli/index.js' # comment, containing comma\n , 'server', '--stdio',\n]\nenabled = false\n";
    let path = fixture.write(".codex/config.toml", source);
    success(&fixture.run(false));
    let updated = fs::read_to_string(&path).expect("updated");
    assert!(updated.contains("# comment, containing comma"));
    assert!(!updated.contains("index.js"));
    let parsed: toml::Value = toml::from_str(&updated).expect("valid TOML");
    assert_eq!(
        parsed["mcp_servers"]["zvec_grep"]["args"]
            .as_array()
            .expect("args")
            .iter()
            .map(|value| value.as_str().expect("string"))
            .collect::<Vec<_>>(),
        ["--server", "--stdio"]
    );
    assert_eq!(
        parsed["mcp_servers"]["zvec_grep"]["enabled"].as_bool(),
        Some(false)
    );
    success(&fixture.run(false));
    assert_eq!(backups(&path).len(), 1);
}

#[test]
fn leaves_empty_and_unconfigured_files_untouched() {
    let fixture = Fixture::new();
    for (path, source) in [
        (".claude.json", "  \n"),
        (".cursor/mcp.json", "{}\n"),
        (".codex/config.toml", "# user notes\n"),
    ] {
        fixture.write(path, source);
    }
    assert!(success(&fixture.run(false)).contains("No existing integrations found"));
    for path in [".claude.json", ".cursor/mcp.json", ".codex/config.toml"] {
        assert!(backups(&fixture.path(path)).is_empty());
    }
}

#[test]
fn migrates_the_managed_npm_bin_path_even_after_it_has_become_a_native_binary() {
    let fixture = Fixture::new();
    let source = json!({"mcpServers":{"zvec_grep":{
        "description":"Managed by zg install", "command":"node",
        "args":[fixture.path("npm prefix/bin/zg"),"server","--stdio"]
    }}});
    let path = fixture.write(".qoder/mcp.json", &source.to_string());
    fixture.write("npm prefix/bin/zg", "replacement native executable");
    success(&fixture.run(false));
    let updated = read_json(&path);
    assert_ne!(updated["mcpServers"]["zvec_grep"]["command"], "node");
    assert_eq!(
        updated["mcpServers"]["zvec_grep"]["args"],
        json!(["--server", "--stdio"])
    );
}

#[test]
fn ambiguous_duplicate_properties_are_reported_without_writing() {
    let fixture = Fixture::new();
    let source = r#"{"mcpServers":{"zvec_grep":{"command":"zg","args":["server","--stdio"],"args":["server","--stdio"]}}}"#;
    let path = fixture.write(".cursor/mcp.json", source);
    let output = fixture.run(false);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Ambiguous configuration properties"));
    assert_eq!(fs::read_to_string(&path).expect("unchanged"), source);
    assert!(backups(&path).is_empty());
}

#[test]
fn migration_does_not_depend_on_global_model_or_daemon_configuration() {
    let fixture = Fixture::new();
    let config = fixture.write(".zvec-grep/config.json", "{invalid global config");
    let path = fixture.write(
        ".cursor/mcp.json",
        r#"{"mcpServers":{"zvec_grep":{"command":"zg","args":["server","--stdio"]}}}"#,
    );
    success(
        &fixture
            .command()
            .args(["--install", "--upgrade-installed"])
            .env("ZVEC_GREP_MCP_TOOLSET", "invalid")
            .env("ZVEC_GREP_SERVER_URL", "not-a-url")
            .output()
            .expect("run"),
    );
    assert_eq!(
        read_json(&path)["mcpServers"]["zvec_grep"]["args"][0],
        "--server"
    );
    assert_eq!(
        fs::read_to_string(config).expect("global config"),
        "{invalid global config"
    );
    assert!(!fixture.path(".zvec-grep/daemon").exists());
}

#[test]
fn already_native_launchers_keep_custom_argument_spelling() {
    let fixture = Fixture::new();
    let source = json!({"mcpServers":{"zvec_grep":{"command":"zg", "args":["--server", "--stdio", format!("--home={}", fixture.path("custom home").display())]}}}).to_string();
    let path = fixture.write(".cursor/mcp.json", &source);
    assert!(success(&fixture.run(false)).contains("Upgraded: 0, compatible: 1"));
    assert_eq!(fs::read_to_string(&path).expect("unchanged"), source);
    assert!(backups(&path).is_empty());
}
