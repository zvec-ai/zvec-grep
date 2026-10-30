use super::*;

#[path = "migration_safety.rs"]
mod migration_safety;

fn instance_record(home: &Path) -> Result<serde_json::Value, Box<dyn Error>> {
    Ok(serde_json::from_slice(&std::fs::read(
        home.join("daemon").join("instance.lock"),
    )?)?)
}

fn write_record(home: &Path, record: &serde_json::Value) -> Result<(), Box<dyn Error>> {
    std::fs::write(
        home.join("daemon").join("instance.lock"),
        serde_json::to_vec(record)?,
    )?;
    Ok(())
}

fn legacy_record(home: &Path) -> Result<serde_json::Value, Box<dyn Error>> {
    let mut record = instance_record(home)?;
    assert_eq!(record["daemonProtocolVersion"], 1);
    let fields = record.as_object_mut().ok_or("instance record object")?;
    // Keep a real process and shutdown endpoint, but publish the Node.js record
    // shape. This exercises migration without requiring Node.js on the runner.
    fields.remove("daemonProtocolVersion");
    fields.remove("listen");
    write_record(home, &record)?;
    Ok(record)
}

fn cli(guard: &ServerGuard, args: &[&str]) -> Command {
    let mut command = Command::new(&guard.binary);
    command
        .args(args)
        .arg("--home")
        .arg(&guard.home)
        .current_dir(&guard.home)
        .env("HOME", &guard.home)
        .env("USERPROFILE", &guard.home)
        .env_remove("ZVEC_GREP_SERVER_TOKEN")
        .env_remove("ZVEC_GREP_SERVER_TOKEN_FILE")
        .env_remove("ZVEC_GREP_MCP_TOOLSET");
    command
}

fn assert_failure(output: &Output, message: &str) {
    assert!(!output.status.success(), "unexpected success: {output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "expected {message}: {output:?}"
    );
}

fn assert_healthy(listen: &str) -> Result<(), Box<dyn Error>> {
    let mut stream = TcpStream::connect(listen)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "GET /healthz HTTP/1.1\r\nHost: {listen}\r\nConnection: close\r\n\r\n"
    )?;
    assert!(read_http_response(&mut stream)?.starts_with("HTTP/1.1 200"));
    Ok(())
}

#[test]
fn legacy_checks_fail_without_requests_or_direct_fallback_and_off_still_works()
-> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let (mut guard, _) = start_server(&binary, &home, "agent", None, |_| {})?;
    let legacy = legacy_record(home.path())?;

    for args in [
        vec!["--server", "status"],
        vec!["--server", "status", "--check-ready"],
        vec!["--status", "--mode", "auto"],
        vec!["--status", "--mode", "server"],
    ] {
        let output = cli(&guard, &args).output()?;
        assert_failure(&output, "legacy zvec-grep daemon");
        assert_failure(&output, "zg --server on");
        assert_eq!(instance_record(home.path())?, legacy);
    }
    assert_healthy(&guard.listen)?;
    assert_command_success(&cli(&guard, &["--status", "--mode", "direct"]).output()?);
    assert_command_success(&guard.stop()?);
    assert!(!home.path().join("daemon").join("instance.lock").exists());

    // A stale legacy record is stopped state, not a compatibility error.
    write_record(home.path(), &legacy)?;
    let stopped = cli(&guard, &["--server", "status"]).output()?;
    assert_command_success(&stopped);
    assert!(String::from_utf8_lossy(&stopped.stdout).contains("Server: stopped"));
    guard.active = true;
    assert_command_success(&cli(&guard, &["--server", "on", "--listen", &guard.listen]).output()?);
    assert_eq!(instance_record(home.path())?["daemonProtocolVersion"], 1);
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn concurrent_starts_replace_one_legacy_daemon_and_reuse_the_new_protocol()
-> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let token_file = home.path().join("token.txt");
    std::fs::write(
        &token_file,
        "migration-token-012345678901234567890123456789",
    )?;
    let (mut guard, _) = start_server(&binary, &home, "full", Some(&token_file), |command| {
        command
            .env_remove("ZVEC_GREP_SERVER_TOKEN")
            .env_remove("ZVEC_GREP_SERVER_TOKEN_FILE");
    })?;
    let legacy = legacy_record(home.path())?;
    let mut children = Vec::new();
    for _ in 0..4 {
        children.push(
            cli(
                &guard,
                &[
                    "--server",
                    "on",
                    "--listen",
                    &guard.listen,
                    "--mcp-toolset",
                    "agent",
                ],
            )
            .arg("--token-file")
            .arg(&token_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
        );
    }
    let outputs = children
        .into_iter()
        .map(Child::wait_with_output)
        .collect::<Result<Vec<_>, _>>()?;
    let current = instance_record(home.path())?;
    assert_eq!(current["daemonProtocolVersion"], 1);
    assert_eq!(current["mcpToolset"], "agent");
    assert_ne!(current["pid"], legacy["pid"]);
    assert_ne!(current["instanceToken"], legacy["instanceToken"]);
    for output in outputs {
        assert_command_success(&output);
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(&format!("PID: {}", current["pid"]))
        );
    }
    assert!(!home.path().join("daemon").join("startup.lock").exists());
    assert_command_success(&cli(&guard, &["--server", "on"]).output()?);
    assert_eq!(instance_record(home.path())?, current);

    let direct = cli(
        &guard,
        &["--status", "--mode", "direct", "--color", "never"],
    )
    .output()?;
    let server = cli(
        &guard,
        &["--status", "--mode", "server", "--color", "never"],
    )
    .env("ZVEC_GREP_SERVER_TOKEN_FILE", &token_file)
    .output()?;
    assert_command_success(&direct);
    assert_command_success(&server);
    assert_eq!(direct.stdout, server.stdout);
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn stdio_bootstrap_replaces_a_legacy_daemon_before_mcp_connects() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let (mut guard, _) = start_server(&binary, &home, "full", None, |_| {})?;
    let legacy = legacy_record(home.path())?;
    let mut bridge = StdioBridge::spawn_with_toolset(&binary, home.path(), &guard.listen, None)?;
    bridge.request(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "protocol-upgrade-test", "version": "1" } }
    }))?;
    bridge.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
    let tools = bridge.request(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))?;
    let mut names = tools["result"]["tools"]
        .as_array()
        .ok_or("tool list")?
        .iter()
        .map(|tool| tool["name"].as_str().ok_or("tool name"))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort_unstable();
    assert_eq!(
        names,
        ["zvec_grep_callees", "zvec_grep_callers", "zvec_grep_search"]
    );
    let current = instance_record(home.path())?;
    assert_eq!(current["daemonProtocolVersion"], 1);
    assert_ne!(current["pid"], legacy["pid"]);
    assert_ne!(current["instanceToken"], legacy["instanceToken"]);
    bridge.close()?;
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn unsupported_protocol_is_rejected_without_replacing_the_daemon() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let (mut guard, _) = start_server(&binary, &home, "agent", None, |_| {})?;
    for version in [0, 2] {
        let mut record = instance_record(home.path())?;
        record["daemonProtocolVersion"] = json!(version);
        write_record(home.path(), &record)?;
        for args in [
            vec!["--server", "status", "--check-ready"],
            vec!["--status", "--mode", "auto"],
            vec!["--status", "--mode", "server"],
            vec!["--server", "on"],
            vec!["--server", "--stdio"],
        ] {
            let output = cli(&guard, &args).stdin(Stdio::null()).output()?;
            assert_failure(&output, &format!("uses daemonProtocolVersion {version}"));
            assert_eq!(instance_record(home.path())?, record);
        }
        assert_healthy(&guard.listen)?;
    }
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn rejected_legacy_shutdown_preserves_the_daemon_until_credentials_are_provided()
-> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let token_file = home.path().join("token.txt");
    std::fs::write(
        &token_file,
        "migration-token-012345678901234567890123456789",
    )?;
    let (mut guard, _) = start_server(&binary, &home, "agent", Some(&token_file), |command| {
        command
            .env_remove("ZVEC_GREP_SERVER_TOKEN")
            .env_remove("ZVEC_GREP_SERVER_TOKEN_FILE");
    })?;
    let legacy = legacy_record(home.path())?;
    for action in ["on", "off"] {
        let mut command = cli(&guard, &["--server", action]);
        if action == "on" {
            command.args(["--listen", &guard.listen]);
        }
        let output = command.output()?;
        assert_failure(&output, "shutdown request failed with HTTP 401");
        assert_eq!(instance_record(home.path())?, legacy);
        assert_healthy(&guard.listen)?;
        assert!(!home.path().join("daemon").join("startup.lock").exists());
    }
    assert_command_success(
        &cli(&guard, &["--server", "on", "--listen", &guard.listen])
            .arg("--token-file")
            .arg(&token_file)
            .output()?,
    );
    assert_eq!(instance_record(home.path())?["daemonProtocolVersion"], 1);
    assert_command_success(&guard.stop()?);
    Ok(())
}
