use super::*;

#[test]
fn invalid_relationship_queries_do_not_start_a_daemon() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let directory = TempDir::new()?;
    let home = directory.path().join("daemon-home");
    for (symbol, limit, message) in [
        (" ", "1", "symbol must contain"),
        ("target", "0", "relationship limit must be positive"),
    ] {
        for tool in ["--callers", "--callees"] {
            let output = Command::new(&binary)
                .args([tool, symbol, "--limit", limit, "--mode", "auto", "--home"])
                .arg(&home)
                .current_dir(directory.path())
                .env("ZVEC_GREP_HOME", &home)
                .output()?;
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains(message));
            assert!(
                !home.exists(),
                "invalid input must not initialize daemon state"
            );
        }
    }
    Ok(())
}

#[test]
fn persisted_calls_match_across_direct_daemon_and_mcp() -> Result<(), Box<dyn Error>> {
    assert_persisted_calls(Path::new("calls.rs"))
}

// macOS test filesystems reject non-UTF-8 filenames on creation.
#[cfg(target_os = "linux")]
#[test]
fn non_unicode_relationship_paths_match_across_direct_daemon_and_mcp() -> Result<(), Box<dyn Error>>
{
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    assert_persisted_calls(&PathBuf::from(OsString::from_vec(
        b"calls-\xff.rs".to_vec(),
    )))
}

fn assert_persisted_calls(file: &Path) -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    std::fs::write(
        workspace.path().join(file),
        "fn target() {\n}\n\nfn caller() {\n    target();\n}\n",
    )?;
    std::fs::create_dir(workspace.path().join("src"))?;
    std::fs::write(
        workspace.path().join("src/main.py"),
        "def nested_entry():\n    pass\n",
    )?;
    let indexed = Command::new(&binary)
        .current_dir(workspace.path())
        .env("ZVEC_GREP_HOME", home.path())
        .env(
            "ZVEC_GREP_WORKSPACE_REGISTRY",
            home.path().join("workspaces.json"),
        )
        .env("ZVEC_GREP_API_KEY", "local-test-key")
        .args([
            "--index",
            "--graph",
            "--mode",
            "direct",
            "--allow-remote",
            "--embedding",
            "qwen/text-embedding-v4",
            "--endpoint",
        ])
        .arg(format!("http://{}/embeddings", embedding.address))
        .output()?;
    assert_command_success(&indexed);
    // Relationship reads must work with the embedding service shut down.
    drop(embedding);
    let (mut guard, _) = start_server(&binary, &home, "agent", None, |_| {})?;
    let mut bridge =
        StdioBridge::spawn_with_toolset(&binary, home.path(), &guard.listen, Some("agent"))?;
    bridge.request(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": {"name": "relationships-test", "version": "1"}}
    }))?;
    bridge.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
    for (id, tool, symbol, endpoint) in [
        (2, "callers", "target", "caller"),
        (3, "callees", "caller", "target"),
    ] {
        let mut replies = Vec::new();
        for mode in ["direct", "server", "auto"] {
            let mut command = Command::new(&binary);
            command
                .arg(format!("--{tool}"))
                .arg(format!(" {symbol} "))
                .arg(workspace.path())
                .args(["--mode", mode, "--home"])
                .arg(home.path());
            let output = command.output()?;
            assert_command_success(&output);
            let text = String::from_utf8(output.stdout)?;
            assert!(text.contains(symbol));
            assert!(text.contains(endpoint));
            assert!(text.contains(&format!("call at {}:5:5", file.display())));
            replies.push(text);
            let (reader, writer) = std::io::pipe()?;
            drop(reader);
            let output = command.stdout(Stdio::from(writer)).output()?;
            assert_command_success(&output);
            assert!(output.stderr.is_empty(), "{tool} {mode}: {output:?}");
        }
        assert!(replies.iter().all(|reply| reply == &replies[0]));
        let mcp = bridge.request(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": format!("zvec_grep_{tool}"), "arguments": {"root": workspace.path(), "symbol": format!(" {symbol} ")}}
        }))?;
        assert_eq!(mcp["result"]["content"][0]["text"], replies[0]);
        assert!(mcp["result"].get("structuredContent").is_none());
    }
    assert_suggestions(&binary, home.path(), workspace.path(), &mut bridge, file)?;
    if file.to_str().is_some() {
        assert_glob_filters(&binary, home.path(), workspace.path(), &mut bridge)?;
    }
    bridge.close()?;
    assert_command_success(&guard.stop()?);
    Ok(())
}

fn assert_glob_filters(
    binary: &Path,
    home: &Path,
    workspace: &Path,
    bridge: &mut StdioBridge,
) -> Result<(), Box<dyn Error>> {
    let nested = workspace.join("src");
    std::fs::create_dir_all(&nested)?;
    for (id, path, matched) in [(50, "calls.rs", true), (51, "missing.py", false)] {
        let mcp = bridge.request(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "zvec_grep_callers", "arguments": {"root": workspace, "symbol": "target", "globs": path}}
        }))?;
        for mode in ["direct", "server"] {
            let output = Command::new(binary)
                .current_dir(workspace)
                .args(["--callers", "target"])
                .arg(workspace)
                .args(["--glob", path, "--mode", mode, "--home"])
                .arg(home)
                .output()?;
            assert_command_success(&output);
            assert_eq!(
                mcp["result"]["content"][0]["text"],
                String::from_utf8(output.stdout)?
            );
        }
        let text = mcp["result"]["content"][0]["text"].as_str().ok_or("text")?;
        if matched {
            assert!(text.starts_with("target  calls.rs:1\n"), "{text}");
        } else {
            assert_eq!(text, "No matches.\n");
        }
    }
    for (mode, symbol, path, expected_path, suggestions) in [
        ("direct", "nested_entry", "main.py", "src/main.py", false),
        ("server", "nested_entry", "main.py", "src/main.py", false),
        ("server", "NESTED_ENTRY", "main.py", "src/main.py", true),
        ("direct", "target", "calls.rs", "calls.rs", false),
    ] {
        let output = Command::new(binary)
            .current_dir(&nested)
            .args([
                "--callers",
                symbol,
                "--glob",
                path,
                "--mode",
                mode,
                "--home",
            ])
            .arg(home)
            .output()?;
        assert_command_success(&output);
        let text = String::from_utf8(output.stdout)?;
        assert!(text.replace('\\', "/").contains(expected_path), "{text}");
        assert_eq!(
            text.contains("Suggestions (retry with an exact name):"),
            suggestions
        );
    }
    Ok(())
}

fn assert_suggestions(
    binary: &Path,
    home: &Path,
    workspace: &Path,
    bridge: &mut StdioBridge,
    file: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut replies = Vec::new();
    for mode in ["direct", "server", "auto"] {
        let output = Command::new(binary)
            .args(["--callers", "WrongScope::TARGET"])
            .arg(workspace)
            .args(["--mode", mode, "--home"])
            .arg(home)
            .output()?;
        assert_command_success(&output);
        replies.push(String::from_utf8(output.stdout)?);
    }
    assert!(replies.iter().all(|reply| reply == &replies[0]));
    let mcp = bridge.request(&json!({
        "jsonrpc": "2.0", "id": 20, "method": "tools/call",
        "params": {"name": "zvec_grep_callers", "arguments": {"root": workspace, "symbol": "WrongScope::TARGET"}}
    }))?;
    let text = mcp["result"]["content"][0]["text"].as_str().ok_or("text")?;
    assert_eq!(text, replies[0]);
    assert!(text.starts_with("No matches."));
    assert!(text.contains(&format!(
        "Suggestions (retry with an exact name):\n  target  {}:1\n",
        file.display()
    )));
    Ok(())
}
