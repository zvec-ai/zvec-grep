use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use serde_json::{Value, json};
use tempfile::TempDir;

fn command(action: &str, root: &Path) -> Command {
    let mut command = base_command(action, root);
    command.args(["--target", "pi"]);
    command
}

fn base_command(action: &str, root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zg"));
    command
        .args([action, "--yes"])
        .current_dir(root)
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("PI_CODING_AGENT_DIR", root.join("pi"))
        .env("ZVEC_GREP_HOME", root.join("runtime"))
        .env("ZVEC_GREP_INSTALL_SKIP_SERVER", "1")
        .env("NO_COLOR", "1")
        .env_remove("ZVEC_GREP_SERVER_URL")
        .env_remove("ZVEC_GREP_MCP_TOOLSET");
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
    String::from_utf8(output.stdout).expect("UTF-8 stdout")
}

fn run_error(command: &mut Command) -> String {
    let output = command.output().expect("run zg");
    assert!(!output.status.success(), "expected failure");
    String::from_utf8(output.stderr).expect("UTF-8 stderr")
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read JSON")).expect("valid JSON")
}

fn write_json(path: &Path, value: &Value) {
    fs::write(
        path,
        serde_json::to_string_pretty(value).expect("serialize"),
    )
    .expect("write JSON");
}

#[test]
fn closed_stdout_allows_installation_and_removal_to_complete() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let path = root.join("pi/mcp.json");
    for action in ["--install", "--uninstall"] {
        let (reader, writer) = std::io::pipe().expect("stdout pipe");
        drop(reader);
        let output = command(action, root)
            .stdout(Stdio::from(writer))
            .output()
            .expect("run with closed stdout");
        assert_eq!(output.status.code(), Some(0), "{action}: {output:?}");
        assert!(output.stderr.is_empty(), "{action}: {output:?}");
        assert_eq!(
            read_json(&path)["mcpServers"]["zvec_grep"].is_object(),
            action == "--install",
            "{action} must complete after its first stdout write"
        );
    }
}

#[test]
fn config_rewrites_preserve_ordered_exposure_rules_and_unrelated_servers() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let home = root.join("pi");
    fs::create_dir_all(&home).expect("mkdir");
    let path = home.join("mcp.json");
    let original = r#"{
        "zCustom": true,
        "mcpServers": {
            "first": {"command":"first", "toolExposure":{"zvec_grep_*":"direct","*":"hidden"}},
            "zvec_grep": {
                "description":"Workspace semantic search and exact text lookup. Managed by zg install.",
                "toolExposure":{"zvec_grep_*":"direct","*":"hidden"}
            },
            "second": {"command":"second", "toolExposure":{"zvec_grep_*":"direct","*":"hidden"}},
            "third": {"command":"third"}
        },
        "aCustom": true,
        "bCustom": true
    }"#;
    fs::write(&path, original).expect("config");
    for action in ["--install", "--install", "--uninstall"] {
        run_ok(&mut command(action, root));
        let source = fs::read_to_string(&path).expect("config");
        for rules in source.split("\"toolExposure\"").skip(1) {
            let rules = &rules[..rules.find('}').expect("rules object")];
            assert!(
                rules.find("\"zvec_grep_*\"").expect("specific rule")
                    < rules.find("\"*\"").expect("fallback rule"),
                "{action} changed first-match exposure rules: {rules}"
            );
        }
        for keys in [
            ["\"first\":", "\"second\":", "\"third\":"],
            ["\"zCustom\":", "\"aCustom\":", "\"bCustom\":"],
        ] {
            let positions = keys.map(|key| source.find(key).expect("preserved key"));
            assert!(positions[0] < positions[1] && positions[1] < positions[2]);
        }
    }
    let expected: Value = serde_json::from_str(original).expect("JSON");
    let removed = read_json(&path);
    for name in ["first", "second", "third"] {
        assert_eq!(removed["mcpServers"][name], expected["mcpServers"][name]);
    }
    fs::write(&path, r#"{
        "zCustom":true,
        "mcpServers":{"zvec_grep":{"description":"Workspace semantic search and exact text lookup. Managed by zg install."}},
        "aCustom":true,
        "bCustom":true
    }"#).expect("single server config");
    run_ok(&mut command("--uninstall", root));
    let source = fs::read_to_string(&path).expect("config");
    assert!(!source.contains("mcpServers"));
    assert!(
        source.find("aCustom").expect("preserved field")
            < source.find("bCustom").expect("preserved field")
    );
}

#[test]
fn explicit_token_replaces_all_authorization_header_casings() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let path = root.join("pi/mcp.json");
    run_ok(command("--install", root).args(["--mcp-transport", "http"]));
    let mut config = read_json(&path);
    let server = &mut config["mcpServers"]["zvec_grep"];
    server["headers"] = json!({
        "authorization": "Bearer old-lowercase",
        "AUTHORIZATION": "Bearer old-uppercase",
        "aUtHoRiZaTiOn": "Bearer old-mixed-case",
        "Authorization": "Bearer old-canonical",
        "X-Custom": "keep"
    });
    server["oauth"] = json!(true);
    server["auth"] = json!({"provider": "saved"});
    write_json(&path, &config);
    run_ok(command("--install", root).args(["--mcp-transport", "http"]));
    assert_eq!(read_json(&path), config);
    run_ok(command("--install", root).args([
        "--mcp-transport",
        "http",
        "--mcp-token-env",
        "NEW_TOKEN",
    ]));
    let updated = read_json(&path);
    let server = &updated["mcpServers"]["zvec_grep"];
    assert_eq!(
        server["headers"],
        json!({"X-Custom": "keep", "Authorization": "Bearer ${NEW_TOKEN}"})
    );
    assert!(server.get("oauth").is_none());
    assert!(server.get("auth").is_none());
}

#[test]
fn inactive_invalid_guidance_does_not_block_installation_or_uninstall() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let home = root.join("pi");
    fs::create_dir_all(&home).expect("mkdir");
    fs::write(home.join("CLAUDE.md"), "# Earlier guidance\n").expect("guidance");
    run_ok(&mut command("--install", root));
    fs::write(home.join("AGENTS.override.md"), "# Active guidance\n").expect("guidance");
    let invalid = [0xff, 0xfe];
    let invalid_path = home.join("AGENTS.md");
    fs::write(&invalid_path, invalid).expect("inactive guidance");
    for _ in 0..2 {
        run_ok(&mut command("--install", root));
        assert!(home.join("mcp.json").exists());
        assert!(
            fs::read_to_string(home.join("AGENTS.override.md"))
                .expect("active guidance")
                .contains("ZVEC_GREP_START")
        );
        assert_eq!(fs::read(&invalid_path).expect("inactive guidance"), invalid);
    }
    for _ in 0..2 {
        let output = command("--uninstall", root).output().expect("run zg");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(output.status.success(), "{stderr}");
        assert!(stderr.contains("warning"), "{stderr}");
        assert!(
            stderr.contains(&invalid_path.display().to_string()),
            "{stderr}"
        );
        assert!(read_json(&home.join("mcp.json"))["mcpServers"]["zvec_grep"].is_null());
        assert_eq!(
            fs::read_to_string(home.join("AGENTS.override.md")).expect("active guidance"),
            "# Active guidance\n"
        );
        assert_eq!(
            fs::read_to_string(home.join("CLAUDE.md")).expect("earlier guidance"),
            "# Earlier guidance\n"
        );
        assert_eq!(fs::read(&invalid_path).expect("inactive guidance"), invalid);
    }
}

#[cfg(unix)]
#[test]
fn unreadable_guidance_is_skipped_when_a_readable_candidate_exists() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let home = root.join("pi");
    fs::create_dir_all(&home).expect("mkdir");
    let unreadable = home.join("AGENTS.override.md");
    fs::write(&unreadable, "# Inaccessible guidance\n").expect("guidance");
    fs::write(home.join("AGENTS.md"), "# Active guidance\n").expect("guidance");
    let permissions = fs::metadata(&unreadable).expect("metadata").permissions();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).expect("permissions");
    let can_read = fs::read(&unreadable).is_ok();
    let output = command("--install", root).output().expect("run zg");
    let active_guidance = fs::read_to_string(home.join("AGENTS.md")).expect("active guidance");
    let uninstall = command("--uninstall", root).output().expect("run zg");
    fs::set_permissions(&unreadable, permissions).expect("restore permissions");
    if can_read {
        return;
    }
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(active_guidance.contains("ZVEC_GREP_START"));
    let stderr = String::from_utf8(uninstall.stderr).expect("UTF-8 stderr");
    assert!(uninstall.status.success(), "{stderr}");
    assert!(stderr.contains("warning"), "{stderr}");
    assert!(
        stderr.contains(&unreadable.display().to_string()),
        "{stderr}"
    );
    assert!(read_json(&home.join("mcp.json"))["mcpServers"]["zvec_grep"].is_null());
    assert_eq!(
        fs::read_to_string(home.join("AGENTS.md")).expect("active guidance"),
        "# Active guidance\n"
    );
    assert_eq!(
        fs::read_to_string(unreadable).expect("guidance"),
        "# Inaccessible guidance\n"
    );
}

#[test]
fn stdio_install_is_idempotent_and_uninstall_preserves_user_content() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let home = root.join("pi");
    fs::create_dir_all(&home).expect("mkdir");
    let path = home.join("mcp.json");
    let other = json!({"command": "other", "args": ["--custom"]});
    write_json(
        &path,
        &json!({"autoEnableCodemode": false, "mcpServers": {"other": other}}),
    );
    fs::write(home.join("AGENTS.md"), "# Existing guidance\n").expect("guidance");

    let stdout = run_ok(&mut command("--install", root));
    assert!(stdout.contains("✓ Pi"));
    assert!(stdout.contains("/reload"));
    let installed = read_json(&path);
    assert_eq!(installed["mcpServers"]["other"], other);
    assert_eq!(installed["autoEnableCodemode"], false);
    let server = &installed["mcpServers"]["zvec_grep"];
    assert_eq!(server["type"], "stdio");
    assert_eq!(server["command"], "zg");
    assert_eq!(server["args"], json!(["--server", "--stdio"]));
    assert_eq!(server["timeout"], 600);
    assert_eq!(server["exposure"], "direct");
    assert!(server.get("cwd").is_none());
    let guidance = fs::read_to_string(home.join("AGENTS.md")).expect("guidance");
    assert!(guidance.contains("mcp__zvec_grep__zvec_grep_search"));
    assert!(guidance.contains("default agent toolset offers search only"));
    assert!(guidance.contains("zg --auth grant"));
    assert!(guidance.contains("do not substitute `--allow-remote`"));
    assert!(guidance.contains("`autoUpdate` to `false`"));
    assert_eq!(guidance.matches("<!-- ZVEC_GREP_START -->").count(), 1);
    let first = fs::read_to_string(&path).expect("config");
    run_ok(&mut command("--install", root));
    assert_eq!(fs::read_to_string(&path).expect("config"), first);
    assert_eq!(
        fs::read_to_string(home.join("AGENTS.md")).expect("guidance"),
        guidance
    );

    run_ok(&mut command("--uninstall", root));
    assert_eq!(
        read_json(&path),
        json!({"autoEnableCodemode": false, "mcpServers": {"other": other}})
    );
    assert_eq!(
        fs::read_to_string(home.join("AGENTS.md")).expect("guidance"),
        "# Existing guidance\n"
    );
    let removed = fs::read_to_string(&path).expect("removed");
    run_ok(&mut command("--uninstall", root));
    assert_eq!(fs::read_to_string(&path).expect("config"), removed);
}

#[test]
fn reinstall_preserves_codemode_saved_without_exposure() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let home = root.join("pi");
    fs::create_dir_all(&home).expect("mkdir");
    let path = home.join("mcp.json");
    let saved = include_str!("fixtures/pi-1.2.0-codemode.json");
    fs::write(&path, saved).expect("saved config");
    for transport in ["stdio", "http"] {
        for force in [false, true] {
            let mut install = command("--install", root);
            install.args(["--mcp-transport", transport]);
            if force {
                install.arg("--force");
            }
            run_ok(&mut install);
            let config = read_json(&path);
            let server = &config["mcpServers"]["zvec_grep"];
            assert_eq!(server["type"], transport);
            assert!(server.get("exposure").is_none(), "{server}");
        }
    }
}

#[test]
fn transport_switch_preserves_ui_choices_and_http_authentication() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let path = root.join("pi/mcp.json");
    run_ok(command("--install", root).args(["--mcp-toolset", "full"]));
    let mut config = read_json(&path);
    let server = &mut config["mcpServers"]["zvec_grep"];
    assert_eq!(
        server["args"],
        json!(["--server", "--stdio", "--mcp-toolset", "full"])
    );
    server["enabled"] = json!(false);
    server["exposure"] = json!("codemode");
    server["toolExposure"] = json!({"zvec_grep_search": "direct"});
    server["env"] = json!({"CUSTOM": "value"});
    server["cwd"] = json!("workspace");
    server["customField"] = json!(true);
    write_json(&path, &config);
    run_ok(command("--install", root).args(["--mcp-toolset", "agent"]));
    let config = read_json(&path);
    let server = &config["mcpServers"]["zvec_grep"];
    assert_eq!(
        server["args"],
        json!(["--server", "--stdio", "--mcp-toolset", "agent"])
    );
    assert_eq!(server["env"], json!({"CUSTOM": "value"}));
    assert_eq!(server["cwd"], "workspace");

    run_ok(command("--install", root).args([
        "--mcp-transport",
        "http",
        "--mcp-toolset",
        "full",
        "--mcp-token-env",
        "TOKEN_ONE",
        "--mcp-tool-timeout",
        "900",
    ]));
    let mut config = read_json(&path);
    let server = &mut config["mcpServers"]["zvec_grep"];
    assert_eq!(server["type"], "http");
    assert_eq!(server["url"], "http://127.0.0.1:7999/mcp");
    assert_eq!(server["headers"]["Authorization"], "Bearer ${TOKEN_ONE}");
    assert_eq!(server["timeout"], 900);
    for key in ["command", "args", "env", "cwd"] {
        assert!(server.get(key).is_none(), "{key}");
    }
    server["headers"]["X-Custom"] = json!("keep");
    write_json(&path, &config);
    run_ok(command("--install", root).args(["--mcp-transport", "http"]));
    assert_eq!(
        read_json(&path)["mcpServers"]["zvec_grep"]["headers"],
        config["mcpServers"]["zvec_grep"]["headers"]
    );
    run_ok(command("--install", root).args([
        "--mcp-transport",
        "http",
        "--mcp-token-env",
        "TOKEN_TWO",
    ]));
    let config = read_json(&path);
    assert_eq!(
        config["mcpServers"]["zvec_grep"]["headers"]["Authorization"],
        "Bearer ${TOKEN_TWO}"
    );
    assert_eq!(
        config["mcpServers"]["zvec_grep"]["headers"]["X-Custom"],
        "keep"
    );

    run_ok(&mut command("--install", root));
    let config = read_json(&path);
    let server = &config["mcpServers"]["zvec_grep"];
    for key in ["url", "headers", "oauth", "auth"] {
        assert!(server.get(key).is_none(), "{key}");
    }
    assert_eq!(server["enabled"], false);
    assert_eq!(server["exposure"], "codemode");
    assert_eq!(
        server["toolExposure"],
        json!({"zvec_grep_search": "direct"})
    );
    assert_eq!(server["customField"], true);
}

#[test]
fn ownership_survives_url_changes_and_damaged_connection_fields() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let path = root.join("pi/mcp.json");
    run_ok(
        command("--install", root)
            .args(["--mcp-transport", "http"])
            .env("ZVEC_GREP_SERVER_URL", "http://127.0.0.1:12345/mcp"),
    );
    run_ok(
        command("--install", root)
            .args(["--mcp-transport", "http"])
            .env("ZVEC_GREP_SERVER_URL", "http://127.0.0.1:12346/mcp"),
    );
    let mut config = read_json(&path);
    assert_eq!(
        config["mcpServers"]["zvec_grep"]["url"],
        "http://127.0.0.1:12346/mcp"
    );
    config["mcpServers"]["zvec_grep"]
        .as_object_mut()
        .expect("object")
        .remove("url");
    write_json(&path, &config);
    fs::create_dir_all(root.join(".zvec-grep")).expect("config dir");
    fs::write(root.join(".zvec-grep/config.json"), "invalid global JSON").expect("global config");
    run_ok(&mut command("--uninstall", root));
    assert_eq!(read_json(&path), json!({}));
}

#[test]
fn unmanaged_entries_require_force_and_are_not_removed_by_uninstall() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let home = root.join("pi");
    fs::create_dir_all(&home).expect("mkdir");
    let path = home.join("mcp.json");
    let original = "{\"mcpServers\":{\"zvec_grep\":{\"command\":\"zg\",\"args\":[\"--server\",\"--stdio\"],\"exposure\":\"hidden\"}}}";
    fs::write(&path, original).expect("config");
    assert!(run_error(&mut command("--install", root)).contains("--force"));
    assert_eq!(fs::read_to_string(&path).expect("config"), original);
    assert!(!home.join("AGENTS.md").exists());
    fs::write(
        home.join("AGENTS.md"),
        "User instructions without final newline",
    )
    .expect("guidance");
    run_ok(&mut command("--uninstall", root));
    assert_eq!(fs::read_to_string(&path).expect("config"), original);
    assert_eq!(
        fs::read_to_string(home.join("AGENTS.md")).expect("guidance"),
        "User instructions without final newline"
    );
    run_ok(command("--install", root).arg("--force"));
    assert_eq!(
        read_json(&path)["mcpServers"]["zvec_grep"]["exposure"],
        "direct"
    );
    run_ok(&mut command("--uninstall", root));
    assert_eq!(read_json(&path), json!({}));
}

#[test]
fn invalid_configuration_and_alias_conflicts_fail_before_any_write() {
    for original in [
        "{invalid}",
        "[]",
        "{\"mcpServers\":[]}",
        "{\"mcpServers\":{\"zvec-grep\":{\"command\":\"other\"}}}",
    ] {
        let temporary = TempDir::new().expect("tempdir");
        let root = temporary.path();
        let home = root.join("pi");
        fs::create_dir_all(&home).expect("mkdir");
        let path = home.join("mcp.json");
        fs::write(&path, original).expect("config");
        for force in [false, true] {
            let mut install = command("--install", root);
            if force {
                install.arg("--force");
            }
            run_error(&mut install);
            assert_eq!(fs::read_to_string(&path).expect("config"), original);
            assert!(!home.join("AGENTS.md").exists());
        }
    }
}

#[test]
fn invalid_http_and_unreadable_guidance_fail_before_config_write() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    fs::create_dir_all(root.join(".zvec-grep")).expect("config dir");
    fs::write(
        root.join(".zvec-grep/config.json"),
        "{\"version\":1,\"server\":{\"host\":\"0.0.0.0\"}}",
    )
    .expect("global config");
    assert!(
        run_error(command("--install", root).args(["--mcp-transport", "http"]))
            .contains("Server listen host must be loopback")
    );
    assert!(!root.join("pi").exists());
    fs::create_dir_all(root.join("pi")).expect("mkdir");
    fs::write(root.join("pi/AGENTS.override.md"), [0xff, 0xfe]).expect("invalid UTF-8");
    assert!(run_error(&mut command("--install", root)).contains("AGENTS.override.md"));
    assert!(!root.join("pi/mcp.json").exists());
}

#[test]
fn guidance_priority_changes_are_cleaned_without_touching_other_files() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let home = root.join("pi");
    fs::create_dir_all(&home).expect("mkdir");
    fs::write(home.join("CLAUDE.md"), "# Lower priority\n").expect("guidance");
    run_ok(&mut command("--install", root));
    assert!(!home.join("AGENTS.md").exists());
    assert!(
        fs::read_to_string(home.join("CLAUDE.md"))
            .expect("guidance")
            .contains("ZVEC_GREP_START")
    );
    fs::write(home.join("AGENTS.override.md"), "# Override\n").expect("guidance");
    fs::write(home.join("AGENTS.md"), "Unmanaged guidance without newline").expect("guidance");
    run_ok(&mut command("--install", root));
    assert!(
        fs::read_to_string(home.join("AGENTS.override.md"))
            .expect("guidance")
            .contains("ZVEC_GREP_START")
    );
    run_ok(&mut command("--uninstall", root));
    assert_eq!(
        fs::read_to_string(home.join("CLAUDE.md")).expect("guidance"),
        "# Lower priority\n"
    );
    assert_eq!(
        fs::read_to_string(home.join("AGENTS.override.md")).expect("guidance"),
        "# Override\n"
    );
    assert_eq!(
        fs::read_to_string(home.join("AGENTS.md")).expect("guidance"),
        "Unmanaged guidance without newline"
    );
}

#[test]
fn configuration_directory_defaults_and_overrides_follow_home_and_cwd() {
    for override_value in [
        None,
        Some(""),
        Some("agent directory"),
        Some("~/custom-agent"),
        Some(" agent directory"),
    ] {
        let temporary = TempDir::new().expect("tempdir");
        let root = temporary.path();
        let expected = match override_value {
            None | Some("") => root.join(".pi/agent"),
            Some("~/custom-agent") => root.join("custom-agent"),
            Some(value) => root.join(value),
        };
        for action in ["--install", "--uninstall"] {
            let mut operation = command(action, root);
            if let Some(value) = override_value {
                operation.env("PI_CODING_AGENT_DIR", value);
            } else {
                operation.env_remove("PI_CODING_AGENT_DIR");
            }
            run_ok(&mut operation);
        }
        assert_eq!(read_json(&expected.join("mcp.json")), json!({}));
    }
}

#[test]
fn unsupported_directory_uri_fails_without_creating_files() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    for action in ["--install", "--uninstall"] {
        assert!(
            run_error(command(action, root).env("PI_CODING_AGENT_DIR", "file:///agent"))
                .contains("native filesystem path")
        );
    }
    assert!(!root.join("pi").exists());
    run_ok(&mut command("--uninstall", root));
    assert!(!root.join("pi").exists());
}

#[cfg(windows)]
#[test]
fn windows_directory_override_accepts_home_paths_and_rejects_shell_drives() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    for action in ["--install", "--uninstall"] {
        run_ok(command(action, root).env("PI_CODING_AGENT_DIR", "~\\custom-agent"));
    }
    assert_eq!(read_json(&root.join("custom-agent/mcp.json")), json!({}));
    for value in ["/c/agent", "/mnt/c/agent", "/cygdrive/c/agent"] {
        for action in ["--install", "--uninstall"] {
            assert!(
                run_error(command(action, root).env("PI_CODING_AGENT_DIR", value))
                    .contains("native Windows path")
            );
        }
    }
    assert!(!root.join("pi").exists());
}

#[test]
fn auto_detects_a_platform_executable_without_creating_user_state_elsewhere() {
    let temporary = TempDir::new().expect("tempdir");
    let root = temporary.path();
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("bin");
    #[cfg(windows)]
    let executable = bin.join("pi.cmd");
    #[cfg(not(windows))]
    let executable = bin.join("pi");
    fs::write(&executable, "unused fixture").expect("executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("permissions");
    }
    let automatic = || {
        let mut operation = base_command("--install", root);
        operation
            .args(["--target", "auto"])
            .env("PATH", &bin)
            .env("PATHEXT", ".CMD;.EXE")
            .env("QODER_IDE_EXECUTABLE", root.join("missing"))
            .env("VSCODE_USER_DIR", root.join("missing-profile"))
            .env("GROK_HOME", root.join("missing-home"));
        operation
    };
    let stdout = run_ok(&mut automatic());
    assert!(stdout.contains("✓ Pi"));
    assert!(root.join("pi/mcp.json").exists());
    fs::remove_file(executable).expect("remove fixture executable");
    assert!(run_ok(&mut automatic()).contains("✓ Pi"));
}
