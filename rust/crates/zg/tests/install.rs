use std::{fs, net::TcpListener, path::Path, process::Command};

use serde_json::Value;
use tempfile::TempDir;

fn zg() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zg"));
    command
        .env("ZVEC_GREP_INSTALL_SKIP_SERVER", "1")
        .env("NO_COLOR", "1");
    command
}

fn run_ok(command: &mut Command) -> String {
    let output = command.output().expect("run zg");
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8 stdout")
}

fn json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read JSON")).expect("valid JSON")
}

#[test]
fn codex_install_and_uninstall_preserve_user_files() {
    let temporary = TempDir::new().expect("tempdir");
    let home = temporary.path().join(".codex");
    fs::create_dir_all(&home).expect("mkdir");
    fs::write(
        home.join("config.toml"),
        "[mcp_servers.other]\ncommand = \"other\"\n# ZVEC_GREP_END\n",
    )
    .expect("config");
    fs::write(home.join("AGENTS.md"), "# Existing instructions\n").expect("guidance");

    let stdout = run_ok(
        zg().args(["--install", "--target", "codex", "--yes"])
            .env("CODEX_HOME", &home),
    );
    assert!(stdout.contains("Installing integrations"));
    let installed = fs::read_to_string(home.join("config.toml")).expect("installed");
    assert!(installed.contains("[mcp_servers.other]"));
    assert!(installed.contains("command = \"zg\""));
    assert!(installed.contains("args = [\"--server\", \"--stdio\"]"));
    assert_eq!(installed.matches("# ZVEC_GREP_START").count(), 1);

    run_ok(
        zg().args(["--uninstall", "--target", "codex", "--yes"])
            .env("CODEX_HOME", &home),
    );
    let config = fs::read_to_string(home.join("config.toml")).expect("uninstalled");
    let guidance = fs::read_to_string(home.join("AGENTS.md")).expect("guidance");
    assert!(config.contains("[mcp_servers.other]"));
    assert!(!config.contains("ZVEC_GREP"));
    assert!(guidance.contains("# Existing instructions"));
    assert!(!guidance.contains("ZVEC_GREP"));
}

#[test]
fn qwen_jsonc_install_is_comment_preserving_and_idempotent() {
    let temporary = TempDir::new().expect("tempdir");
    let home = temporary.path().join(".qwen");
    fs::create_dir_all(&home).expect("mkdir");
    let path = home.join("settings.json");
    fs::write(
        &path,
        "{\n  // Keep theme.\n  \"theme\": \"dark\",\n  /* Keep server. */\n  \"mcpServers\": {\n    \"other\": { \"httpUrl\": \"https://example.test/mcp\" }\n  }\n}\n",
    )
    .expect("settings");

    run_ok(
        zg().args([
            "--install",
            "--target",
            "qwen",
            "--mcp-toolset",
            "full",
            "--yes",
        ])
        .env("QWEN_HOME", &home),
    );
    let first = fs::read_to_string(&path).expect("installed");
    assert!(first.contains("// Keep theme."));
    assert!(first.contains("/* Keep server. */"));
    assert_eq!(
        jsonc(&first)["mcpServers"]["zvec_grep"]["args"],
        serde_json::json!(["--server", "--stdio", "--mcp-toolset", "full"])
    );
    run_ok(
        zg().args([
            "--install",
            "--target",
            "qwen",
            "--mcp-toolset",
            "full",
            "--yes",
        ])
        .env("QWEN_HOME", &home),
    );
    assert_eq!(fs::read_to_string(&path).expect("second install"), first);

    run_ok(
        zg().args(["--uninstall", "--target", "qwen", "--yes"])
            .env("QWEN_HOME", &home),
    );
    let removed = fs::read_to_string(&path).expect("removed");
    assert!(removed.contains("// Keep theme."));
    assert!(jsonc(&removed)["mcpServers"].get("zvec_grep").is_none());
    assert_eq!(jsonc(&removed)["theme"], "dark");
}

#[test]
fn qoder_manages_owned_permissions_and_both_clients() {
    let temporary = TempDir::new().expect("tempdir");
    let home = temporary.path().join(".qoder");
    let ide = home.join("mcp.json");
    fs::create_dir_all(&home).expect("mkdir");
    fs::write(
        home.join("settings.json"),
        "{\n  // policy\n  \"permissions\": {\n    \"allow\": [\"Bash(git status)\"]\n  }\n}\n",
    )
    .expect("settings");

    run_ok(
        zg().args(["--install", "--target", "qoder", "--yes"])
            .env("QODER_CONFIG_DIR", &home)
            .env("QODER_IDE_MCP_PATH", &ide),
    );
    let settings = fs::read_to_string(home.join("settings.json")).expect("settings");
    let parsed = jsonc(&settings);
    assert!(settings.contains("// policy"));
    assert!(
        parsed["permissions"]["allow"]
            .as_array()
            .expect("allow")
            .iter()
            .any(|rule| rule == "mcp__zvec_grep__zvec_grep_search")
    );
    assert_eq!(parsed["mcpServers"]["zvec_grep"]["trust"], true);
    assert!(
        json(&ide)["mcpServers"]["zvec_grep"]["command"]
            .as_str()
            .is_some_and(|command| Path::new(command).is_absolute())
    );

    run_ok(
        zg().args(["--uninstall", "--target", "qoder", "--yes"])
            .env("QODER_CONFIG_DIR", &home)
            .env("QODER_IDE_MCP_PATH", &ide),
    );
    let removed = jsonc(&fs::read_to_string(home.join("settings.json")).expect("removed"));
    assert_eq!(
        removed["permissions"]["allow"],
        serde_json::json!(["Bash(git status)"])
    );
    assert!(removed["mcpServers"].get("zvec_grep").is_none());
    assert!(json(&ide).get("mcpServers").is_none());
}

#[test]
fn qoder_trims_ide_environment_paths() {
    let temporary = TempDir::new().expect("tempdir");
    let home = temporary.path().join(".qoder");
    let ide = home.join("mcp.json");
    let executable = temporary.path().join("Qoder IDE");
    let empty_path = temporary.path().join("empty-bin");
    fs::create_dir_all(&empty_path).expect("mkdir");
    fs::write(&executable, "#!/bin/sh\n").expect("executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&executable).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).expect("permissions");
    }

    let stdout = run_ok(
        zg().args(["--install", "--yes"])
            .env("PATH", &empty_path)
            .env("HOME", temporary.path())
            .env("USERPROFILE", temporary.path())
            .env("QODER_CONFIG_DIR", &home)
            .env(
                "QODER_IDE_EXECUTABLE",
                format!("  {}  ", executable.display()),
            )
            .env("QODER_IDE_MCP_PATH", format!("  {}  ", ide.display())),
    );

    assert!(stdout.contains("Qoder"));
    assert!(home.join("settings.json").is_file());
    assert!(ide.is_file());
}

#[test]
fn qoder_force_drops_unmanaged_always_allow_tools() {
    let temporary = TempDir::new().expect("tempdir");
    let home = temporary.path().join(".qoder");
    let settings = home.join("settings.json");
    let ide = home.join("mcp.json");
    fs::create_dir_all(&home).expect("mkdir");
    fs::write(
        &settings,
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                "zvec_grep": {
                    "type": "http",
                    "url": "https://example.test/user-owned-mcp",
                    "alwaysAllow": ["user_tool", "zvec_grep_search"]
                }
            },
            "permissions": {
                "allow": ["mcp__zvec_grep__zvec_grep_search"]
            }
        }))
        .expect("serialize"),
    )
    .expect("settings");

    run_ok(
        zg().args(["--install", "--target", "qoder", "--yes", "--force"])
            .env("QODER_CONFIG_DIR", &home)
            .env("QODER_IDE_MCP_PATH", &ide),
    );

    let installed = json(&settings);
    assert_eq!(
        installed["mcpServers"]["zvec_grep"]["alwaysAllow"],
        serde_json::json!(["zvec_grep_search", "zvec_grep_rg"])
    );
}

#[test]
fn http_token_requires_an_explicit_http_transport() {
    let output = zg()
        .args([
            "--install",
            "--target",
            "codex",
            "--mcp-token-env",
            "TOKEN",
            "--yes",
        ])
        .output()
        .expect("run zg");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires --mcp-transport http"));
}

#[test]
fn claude_opencode_and_cursor_match_the_typescript_shapes() {
    let temporary = TempDir::new().expect("tempdir");
    let claude = temporary.path().join("claude");
    let opencode = temporary.path().join("opencode.json");
    let cursor = temporary.path().join("cursor");
    fs::create_dir_all(&claude).expect("claude dir");
    fs::write(
        claude.join(".claude.json"),
        "{\"theme\":\"dark\",\"mcpServers\":{\"other\":{\"url\":\"https://example.test\"}}}\n",
    )
    .expect("claude config");
    fs::write(
        claude.join("settings.json"),
        "{\"permissions\":{\"allow\":[\"Bash(git status)\"],\"deny\":[\"Bash(rm *)\"]}}\n",
    )
    .expect("claude settings");
    fs::write(
        &opencode,
        "{\"model\":\"custom/model\",\"mcp\":{\"other\":{\"type\":\"remote\",\"url\":\"https://example.test\"}}}\n",
    )
    .expect("opencode config");

    run_ok(
        zg().args(["--install", "--target", "claude", "--yes"])
            .env("CLAUDE_CONFIG_DIR", &claude),
    );
    run_ok(
        zg().args([
            "--install",
            "--target",
            "opencode",
            "--mcp-transport",
            "http",
            "--mcp-token-env",
            "TOKEN",
            "--yes",
        ])
        .env("OPENCODE_CONFIG", &opencode),
    );
    run_ok(
        zg().args([
            "--install",
            "--target",
            "cursor",
            "--mcp-transport",
            "http",
            "--mcp-token-env",
            "TOKEN",
            "--yes",
        ])
        .env("CURSOR_CONFIG_DIR", &cursor),
    );

    let claude_mcp = json(&claude.join(".claude.json"));
    assert_eq!(claude_mcp["theme"], "dark");
    assert_eq!(claude_mcp["mcpServers"]["zvec_grep"]["command"], "zg");
    let claude_settings = json(&claude.join("settings.json"));
    assert!(
        claude_settings["permissions"]["allow"]
            .as_array()
            .expect("allow")
            .iter()
            .any(|rule| rule == "mcp__zvec_grep__*")
    );
    assert_eq!(
        claude_settings["permissions"]["deny"],
        serde_json::json!(["Bash(rm *)"])
    );
    assert_eq!(
        json(&opencode)["mcp"]["zvec_grep"]["headers"]["Authorization"],
        "Bearer {env:TOKEN}"
    );
    assert_eq!(
        json(&cursor.join("mcp.json"))["mcpServers"]["zvec_grep"]["headers"]["Authorization"],
        "Bearer ${TOKEN}"
    );

    run_ok(
        zg().args(["--uninstall", "--target", "claude", "--yes"])
            .env("CLAUDE_CONFIG_DIR", &claude),
    );
    run_ok(
        zg().args(["--uninstall", "--target", "opencode", "--yes"])
            .env("OPENCODE_CONFIG", &opencode),
    );
    run_ok(
        zg().args(["--uninstall", "--target", "cursor", "--yes"])
            .env("CURSOR_CONFIG_DIR", &cursor),
    );
    assert!(
        json(&claude.join(".claude.json"))["mcpServers"]
            .get("zvec_grep")
            .is_none()
    );
    assert!(json(&opencode)["mcp"]["zvec_grep"].is_null());
    assert!(json(&cursor.join("mcp.json")).get("mcpServers").is_none());
}

#[test]
fn install_starts_the_configured_server_with_the_selected_toolset() {
    let temporary = TempDir::new().expect("tempdir");
    let home = temporary.path().join("home");
    let runtime_home = temporary.path().join("runtime");
    let codex = temporary.path().join("codex");
    let listener = TcpListener::bind("127.0.0.1:0").expect("available port");
    let port = listener.local_addr().expect("local address").port();
    drop(listener);
    fs::create_dir_all(home.join(".zvec-grep")).expect("config dir");
    fs::write(
        home.join(".zvec-grep/config.json"),
        format!("{{\"version\":1,\"client\":{{\"serverUrl\":\"http://127.0.0.1:{port}/mcp\"}},\"server\":{{\"host\":\"127.0.0.1\",\"port\":{port}}}}}\n"),
    )
    .expect("global config");

    let output = run_ok(
        zg().args([
            "--install",
            "--target",
            "codex",
            "--mcp-toolset",
            "full",
            "--yes",
        ])
        .env_remove("ZVEC_GREP_INSTALL_SKIP_SERVER")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CODEX_HOME", &codex)
        .env("ZVEC_GREP_HOME", &runtime_home),
    );
    assert!(output.contains(&format!("ready at http://127.0.0.1:{port}/mcp")));
    let status = run_ok(
        Command::new(env!("CARGO_BIN_EXE_zg"))
            .args(["--server", "status", "--check-ready", "--home"])
            .arg(&runtime_home),
    );
    assert!(status.contains("MCP toolset: full"));
    let config = fs::read_to_string(codex.join("config.toml")).expect("codex config");
    assert!(config.contains("args = [\"--server\", \"--stdio\", \"--mcp-toolset\", \"full\"]"));
    let second_codex = temporary.path().join("second-codex");
    run_ok(
        zg().args(["--install", "--target", "codex", "--yes"])
            .env_remove("ZVEC_GREP_INSTALL_SKIP_SERVER")
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("CODEX_HOME", &second_codex)
            .env("ZVEC_GREP_HOME", &runtime_home),
    );
    let still_full = run_ok(
        Command::new(env!("CARGO_BIN_EXE_zg"))
            .args(["--server", "status", "--check-ready", "--home"])
            .arg(&runtime_home),
    );
    assert!(still_full.contains("MCP toolset: full"));
    run_ok(
        Command::new(env!("CARGO_BIN_EXE_zg"))
            .args(["--server", "off", "--home"])
            .arg(&runtime_home),
    );
}

fn jsonc(source: &str) -> Value {
    let mut stripped = String::with_capacity(source.len());
    let mut characters = source.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(character) = characters.next() {
        if in_string {
            stripped.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == '"' {
            in_string = true;
            stripped.push(character);
        } else if character == '/' && characters.peek() == Some(&'/') {
            stripped.push(' ');
            stripped.push(' ');
            characters.next();
            for next in characters.by_ref() {
                if next == '\n' {
                    stripped.push('\n');
                    break;
                }
                stripped.push(' ');
            }
        } else if character == '/' && characters.peek() == Some(&'*') {
            stripped.push(' ');
            stripped.push(' ');
            characters.next();
            while let Some(next) = characters.next() {
                if next == '*' && characters.peek() == Some(&'/') {
                    stripped.push(' ');
                    stripped.push(' ');
                    characters.next();
                    break;
                }
                stripped.push(if next == '\n' { '\n' } else { ' ' });
            }
        } else {
            stripped.push(character);
        }
    }
    serde_json::from_str(&stripped).expect("valid JSONC")
}

fn opencode_command(action: &str, root: &Path) -> Command {
    let mut command = zg();
    command
        .args([action, "--target", "opencode", "--yes"])
        .env_remove("OPENCODE_CONFIG")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("HOME", root)
        .env("USERPROFILE", root);
    command
}

#[test]
fn opencode_jsonc_preserves_comments_trailing_commas_and_other_settings() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let directory = root.join("config").join("opencode");
    fs::create_dir_all(&directory).expect("mkdir");
    let path = directory.join("opencode.jsonc");
    let source = "{\n  // Keep model.\n  \"model\": \"custom/model\",\n  \"array\": [\"literal ,} and ,]\",],\n  \"mcp\": {\n    /* Keep other server. */\n    \"other\": {\"type\": \"remote\", \"url\": \"https://example.test/mcp\",},\n  },\n}\n";
    fs::write(&path, source).expect("write");
    let stdout = run_ok(&mut opencode_command("--install", root));
    assert!(
        stdout.contains(&format!("Config    {}", path.display())),
        "expected configuration path {}, stdout:\n{stdout}",
        path.display()
    );
    assert!(!directory.join("opencode.json").exists());
    let installed = fs::read_to_string(&path).expect("read");
    assert!(installed.contains("\"zvec_grep\""));
    run_ok(&mut opencode_command("--install", root));
    assert_eq!(fs::read_to_string(&path).expect("read"), installed);
    run_ok(&mut opencode_command("--uninstall", root));
    let removed = fs::read_to_string(&path).expect("read");
    assert!(!removed.contains("\"zvec_grep\""));
    for line in source.lines().filter(|line| {
        line.contains("Keep")
            || line.contains("\"model\"")
            || line.contains("\"array\"")
            || line.contains("\"other\"")
    }) {
        assert!(installed.contains(line));
        assert!(removed.contains(line));
    }
    run_ok(&mut opencode_command("--install", root));
}

#[test]
fn opencode_selects_jsonc_and_cleans_both_global_files() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let directory = root.join("config").join("opencode");
    fs::create_dir_all(&directory).expect("mkdir");
    let json_path = directory.join("opencode.json");
    let jsonc_path = directory.join("opencode.jsonc");
    let legacy = "{\"model\":\"json/model\",\"mcp\":{\"zvec_grep\":{\"type\":\"remote\",\"url\":\"http://127.0.0.1:7999/mcp\",\"enabled\":true},\"other\":{\"url\":\"https://example.test/mcp\"}}}\n";
    fs::write(&json_path, legacy).expect("write");
    fs::write(
        &jsonc_path,
        "{\n  // Active config\n  \"model\": \"jsonc/model\"\n}\n",
    )
    .expect("write");
    let stdout = run_ok(&mut opencode_command("--install", root));
    assert!(
        stdout.contains("both opencode.jsonc and opencode.json exist; selected opencode.jsonc")
    );
    assert_eq!(fs::read_to_string(&json_path).expect("read"), legacy);
    run_ok(&mut opencode_command("--uninstall", root));
    assert!(json(&json_path)["mcp"].get("zvec_grep").is_none());
    assert_eq!(
        json(&json_path)["mcp"]["other"]["url"],
        "https://example.test/mcp"
    );
    let removed = fs::read_to_string(&jsonc_path).expect("read");
    assert!(removed.contains("// Active config"));
    assert_eq!(jsonc(&removed)["mcp"], serde_json::json!({}));
}

#[test]
fn opencode_explicit_override_is_trimmed_and_scopes_uninstall() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    run_ok(&mut opencode_command("--install", root));
    let global = root.join("config/opencode/opencode.json");
    let original = fs::read_to_string(&global).expect("read");
    let custom = root.join("custom.jsonc");
    fs::write(&custom, "{\n // Keep custom\n}\n").expect("write");
    for action in ["--install", "--uninstall"] {
        run_ok(
            opencode_command(action, root)
                .env("OPENCODE_CONFIG", "  custom.jsonc  ")
                .current_dir(root),
        );
        assert_eq!(fs::read_to_string(&global).expect("read"), original);
    }
    assert!(
        !fs::read_to_string(&custom)
            .expect("read")
            .contains("\"zvec_grep\"")
    );
    // Blank overrides fall back to the home configuration directory.
    run_ok(
        opencode_command("--install", root)
            .env("OPENCODE_CONFIG", " ")
            .env("XDG_CONFIG_HOME", " "),
    );
    assert!(root.join(".config/opencode/opencode.json").exists());
}

#[test]
fn opencode_jsonc_conflicts_and_invalid_containers_do_not_modify_files() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let path = root.join("custom.jsonc");
    let unmanaged = "{\n // Keep unmanaged\n \"mcp\": {\"zvec_grep\": {\"url\": \"https://example.test/unmanaged\"},},\n}\n";
    fs::write(&path, unmanaged).expect("write");
    let output = opencode_command("--install", root)
        .env("OPENCODE_CONFIG", &path)
        .output()
        .expect("run");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--force"));
    assert_eq!(fs::read_to_string(&path).expect("read"), unmanaged);
    run_ok(opencode_command("--uninstall", root).env("OPENCODE_CONFIG", &path));
    assert_eq!(fs::read_to_string(&path).expect("read"), unmanaged);
    run_ok(
        opencode_command("--install", root)
            .env("OPENCODE_CONFIG", &path)
            .arg("--force"),
    );
    assert!(
        fs::read_to_string(&path)
            .expect("read")
            .contains("// Keep unmanaged")
    );
    for source in ["{\"mcp\":null}", "{\"mcp\":[]}", "{,}", "{\"mcp\": {,,}}"] {
        fs::write(&path, source).expect("write");
        for action in ["--install", "--uninstall"] {
            let output = opencode_command(action, root)
                .env("OPENCODE_CONFIG", &path)
                .output()
                .expect("run");
            assert!(!output.status.success(), "{action}: {source}");
            assert_eq!(fs::read_to_string(&path).expect("read"), source);
        }
    }
}

fn copilot_command(action: &str, root: &Path) -> Command {
    let mut command = zg();
    command
        .args([action, "--target", "copilot", "--yes"])
        .env("COPILOT_HOME", root.join("copilot"))
        .env("HOME", root)
        .env("USERPROFILE", root);
    command
}

fn vscode_command(action: &str, root: &Path) -> Command {
    let mut command = zg();
    command
        .args([action, "--target", "vscode", "--yes"])
        .env("VSCODE_USER_DIR", root.join("profile"))
        .env("COPILOT_HOME", root.join("copilot"))
        .env("HOME", root)
        .env("USERPROFILE", root);
    command
}

#[test]
fn copilot_installs_and_removes_managed_configuration() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let home = root.join("copilot");
    fs::create_dir_all(&home).expect("mkdir");
    fs::write(
        home.join("mcp-config.json"),
        "{\"mcpServers\":{\"other\":{\"command\":\"npx\"}}}\n",
    )
    .expect("config");
    fs::write(home.join("copilot-instructions.md"), "# My instructions\n").expect("guidance");

    let stdout = run_ok(copilot_command("--install", root).args([
        "--mcp-toolset",
        "full",
        "--mcp-tool-timeout",
        "900",
    ]));
    let path = home.join("mcp-config.json");
    assert!(stdout.contains(&format!("Config    {}", path.display())));
    let installed = json(&path);
    assert_eq!(installed["mcpServers"]["other"]["command"], "npx");
    assert_eq!(
        installed["mcpServers"]["zvec_grep"],
        serde_json::json!({
            "type": "local", "command": "zg",
            "args": ["--server", "--stdio", "--mcp-toolset", "full"],
            "tools": ["*"], "timeout": 900_000
        })
    );
    let guidance = fs::read_to_string(home.join("copilot-instructions.md")).expect("guidance");
    assert!(guidance.contains("# My instructions"));
    assert!(guidance.contains("<!-- ZVEC_GREP_START -->"));

    run_ok(copilot_command("--install", root).args([
        "--mcp-transport",
        "http",
        "--mcp-token-env",
        "TOKEN",
        "--mcp-tool-timeout",
        "45",
    ]));
    let http = json(&path);
    assert_eq!(
        http["mcpServers"]["zvec_grep"],
        serde_json::json!({
            "type": "http", "url": "http://127.0.0.1:7999/mcp",
            "headers": {"Authorization": "Bearer ${TOKEN}"},
            "tools": ["*"], "timeout": 45000
        })
    );
    run_ok(&mut copilot_command("--uninstall", root));
    assert!(json(&path)["mcpServers"].get("zvec_grep").is_none());
    assert!(
        fs::read_to_string(home.join("copilot-instructions.md"))
            .expect("guidance")
            .contains("# My instructions")
    );
}

#[test]
fn vscode_preserves_jsonc_and_shared_guidance_lifecycle() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let profile = root.join("profile");
    let copilot = root.join("copilot");
    fs::create_dir_all(&profile).expect("mkdir");
    fs::create_dir_all(copilot.join("instructions")).expect("mkdir");
    let config = profile.join("mcp.json");
    let guidance = copilot.join("instructions/zvec-grep.instructions.md");
    fs::write(&config, "{\n  // Keep server\n  \"servers\": {\"other\": {\"command\": \"npx\"},},\n  \"inputs\": [],\n}\n").expect("config");
    fs::write(&guidance, "My notes.\n").expect("guidance");

    run_ok(&mut vscode_command("--install", root));
    let installed = fs::read_to_string(&config).expect("config");
    assert!(installed.contains("// Keep server"));
    assert!(installed.contains("\"zvec_grep\""));
    assert!(installed.contains("\"type\": \"stdio\""));
    assert_eq!(
        json(&copilot.join("mcp-config.json"))["mcpServers"]["zvec_grep"]["timeout"],
        600_000
    );
    let instructions = fs::read_to_string(&guidance).expect("guidance");
    assert!(instructions.starts_with("---\napplyTo: '**'\n---\n"));
    assert!(instructions.contains("My notes."));
    run_ok(&mut vscode_command("--install", root));
    assert_eq!(fs::read_to_string(&config).expect("config"), installed);
    assert_eq!(
        fs::read_to_string(&guidance).expect("guidance"),
        instructions
    );

    run_ok(&mut vscode_command("--uninstall", root));
    let removed = fs::read_to_string(&config).expect("config");
    assert!(removed.contains("// Keep server"));
    assert!(!removed.contains("\"zvec_grep\""));
    assert_eq!(
        fs::read_to_string(&guidance).expect("guidance"),
        "\nMy notes.\n"
    );
    assert!(
        json(&copilot.join("mcp-config.json"))
            .get("mcpServers")
            .is_none()
    );
}

#[test]
fn vscode_preflights_conflicts_and_keeps_copilot_entry_until_both_targets_leave() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let profile = root.join("profile");
    let copilot = root.join("copilot");
    fs::create_dir_all(&profile).expect("mkdir");
    let config = profile.join("mcp.json");
    let unmanaged = "{\"servers\":{\"zvec_grep\":{\"command\":\"other\"}}}\n";
    fs::write(&config, unmanaged).expect("config");
    let output = vscode_command("--install", root).output().expect("run");
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&config).expect("config"), unmanaged);
    assert!(!copilot.join("mcp-config.json").exists());

    run_ok(vscode_command("--install", root).arg("--force"));
    run_ok(&mut copilot_command("--install", root));
    run_ok(&mut copilot_command("--uninstall", root));
    assert!(
        json(&copilot.join("mcp-config.json"))["mcpServers"]
            .get("zvec_grep")
            .is_some()
    );
    run_ok(&mut vscode_command("--uninstall", root));
    assert!(
        json(&copilot.join("mcp-config.json"))
            .get("mcpServers")
            .is_none()
    );
}

#[test]
fn vscode_configures_all_existing_profiles_and_preflights_guidance() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let appdata = root.join("appdata");
    let stable = appdata.join("Code/User");
    let insiders = appdata.join("Code - Insiders/User");
    let copilot = root.join("copilot");
    let guidance = copilot.join("instructions/zvec-grep.instructions.md");
    fs::create_dir_all(&stable).expect("mkdir");
    fs::create_dir_all(&insiders).expect("mkdir");
    fs::create_dir_all(guidance.parent().expect("parent")).expect("mkdir");
    fs::write(&guidance, "---\napplyTo: 'src/**'\n---\nMy notes.\n").expect("guidance");
    let mut install = zg();
    install
        .args(["--install", "--target", "vscode", "--yes"])
        .env_remove("VSCODE_USER_DIR")
        .env_remove("VSCODE_PORTABLE")
        .env("VSCODE_APPDATA", &appdata)
        .env("COPILOT_HOME", &copilot)
        .env("HOME", root)
        .env("USERPROFILE", root);
    let output = install.output().expect("run");
    assert!(!output.status.success());
    assert!(!stable.join("mcp.json").exists());
    assert!(!insiders.join("mcp.json").exists());
    assert!(!copilot.join("mcp-config.json").exists());

    fs::write(&guidance, "My notes.\n").expect("guidance");
    run_ok(&mut install);
    for profile in [&stable, &insiders] {
        assert!(
            jsonc(&fs::read_to_string(profile.join("mcp.json")).expect("config"))["servers"]
                .get("zvec_grep")
                .is_some()
        );
    }
    let mut uninstall = zg();
    uninstall
        .args(["--uninstall", "--target", "vscode", "--yes"])
        .env_remove("VSCODE_USER_DIR")
        .env_remove("VSCODE_PORTABLE")
        .env("VSCODE_APPDATA", &appdata)
        .env("COPILOT_HOME", &copilot)
        .env("HOME", root)
        .env("USERPROFILE", root);
    run_ok(&mut uninstall);
    for profile in [&stable, &insiders] {
        assert!(
            jsonc(&fs::read_to_string(profile.join("mcp.json")).expect("config"))["servers"]
                .get("zvec_grep")
                .is_none()
        );
    }
}

#[test]
fn vscode_portable_http_uses_supported_schema_and_env_token() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let mut install = zg();
    install
        .args([
            "--install",
            "--target",
            "vscode",
            "--mcp-transport",
            "http",
            "--mcp-token-env",
            "TOKEN",
            "--mcp-tool-timeout",
            "900",
            "--yes",
        ])
        .env_remove("VSCODE_USER_DIR")
        .env("VSCODE_PORTABLE", root)
        .env("COPILOT_HOME", root.join("copilot"))
        .env("HOME", root)
        .env("USERPROFILE", root);
    run_ok(&mut install);
    let profile = json(&root.join("user-data/User/mcp.json"));
    assert_eq!(
        profile["servers"]["zvec_grep"],
        serde_json::json!({
            "type": "http", "url": "http://127.0.0.1:7999/mcp",
            "headers": {"Authorization": "Bearer ${env:TOKEN}"}
        })
    );
    let copilot = json(&root.join("copilot/mcp-config.json"));
    assert_eq!(copilot["mcpServers"]["zvec_grep"]["timeout"], 900_000);
    assert_eq!(
        copilot["mcpServers"]["zvec_grep"]["headers"]["Authorization"],
        "Bearer ${TOKEN}"
    );
}
