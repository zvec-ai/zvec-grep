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
        zg().args(["install", "--target", "codex", "--yes"])
            .env("CODEX_HOME", &home),
    );
    assert!(stdout.contains("Installing integrations"));
    let installed = fs::read_to_string(home.join("config.toml")).expect("installed");
    assert!(installed.contains("[mcp_servers.other]"));
    assert!(installed.contains("command = \"zg\""));
    assert!(installed.contains("args = [\"server\", \"--stdio\"]"));
    assert_eq!(installed.matches("# ZVEC_GREP_START").count(), 1);

    run_ok(
        zg().args(["uninstall", "--target", "codex", "--yes"])
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
            "install",
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
        serde_json::json!(["server", "--stdio", "--mcp-toolset", "full"])
    );
    run_ok(
        zg().args([
            "install",
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
        zg().args(["uninstall", "--target", "qwen", "--yes"])
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
        zg().args(["install", "--target", "qoder", "--yes"])
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
        zg().args(["uninstall", "--target", "qoder", "--yes"])
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
fn http_token_requires_an_explicit_http_transport() {
    let output = zg()
        .args([
            "install",
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
        zg().args(["install", "--target", "claude", "--yes"])
            .env("CLAUDE_CONFIG_DIR", &claude),
    );
    run_ok(
        zg().args([
            "install",
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
            "install",
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
        zg().args(["uninstall", "--target", "claude", "--yes"])
            .env("CLAUDE_CONFIG_DIR", &claude),
    );
    run_ok(
        zg().args(["uninstall", "--target", "opencode", "--yes"])
            .env("OPENCODE_CONFIG", &opencode),
    );
    run_ok(
        zg().args(["uninstall", "--target", "cursor", "--yes"])
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
            "install",
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
            .args(["server", "status", "--check-ready", "--home"])
            .arg(&runtime_home),
    );
    assert!(status.contains("MCP toolset: full"));
    let config = fs::read_to_string(codex.join("config.toml")).expect("codex config");
    assert!(config.contains("args = [\"server\", \"--stdio\", \"--mcp-toolset\", \"full\"]"));
    let second_codex = temporary.path().join("second-codex");
    run_ok(
        zg().args(["install", "--target", "codex", "--yes"])
            .env_remove("ZVEC_GREP_INSTALL_SKIP_SERVER")
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("CODEX_HOME", &second_codex)
            .env("ZVEC_GREP_HOME", &runtime_home),
    );
    let still_full = run_ok(
        Command::new(env!("CARGO_BIN_EXE_zg"))
            .args(["server", "status", "--check-ready", "--home"])
            .arg(&runtime_home),
    );
    assert!(still_full.contains("MCP toolset: full"));
    run_ok(
        Command::new(env!("CARGO_BIN_EXE_zg"))
            .args(["server", "off", "--home"])
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
