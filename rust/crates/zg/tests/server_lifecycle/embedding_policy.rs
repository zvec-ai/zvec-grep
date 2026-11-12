use super::*;

const CODE: &str = "local/potion-code-16m-v2";
const TEXT: &str = "local/bekko-256-int8";

fn isolate(command: &mut Command, home: &Path) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ZVEC_GREP_") {
            command.env_remove(key);
        }
    }
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ZVEC_GREP_MODEL_CACHE", home.join("models"));
}

fn workspace_files(root: &Path, text: bool) -> std::io::Result<()> {
    // Nonempty whitespace exercises size weights without loading embedding models.
    std::fs::write(root.join("large.rs"), " ".repeat(512 * 1024))?;
    if text {
        for name in ["one.md", "two.md"] {
            std::fs::write(root.join(name), "\n".repeat(48 * 1024))?;
        }
    }
    Ok(())
}

fn cli_index(
    guard: &ServerGuard,
    root: &Path,
    mode: &str,
    rebuild: bool,
) -> Result<(), Box<dyn Error>> {
    let mut command = Command::new(&guard.binary);
    isolate(&mut command, &guard.home);
    command
        .current_dir(root)
        .args(["--index", "--mode", mode, "--home"])
        .arg(&guard.home)
        .args(["--color", "never"]);
    if rebuild {
        command.arg("--rebuild");
    }
    let output = command.output()?;
    assert_command_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(if rebuild {
            "existing index"
        } else {
            "Using recommended"
        }),
        "{stderr}"
    );
    Ok(())
}

fn mcp_index(port: u16, root: &Path, rebuild: bool) -> Result<(), Box<dyn Error>> {
    let response = rpc(&modern_post(
        port,
        "tools/call",
        json!({
            "name": "zvec_grep_index", "arguments": {"root": root, "wait": true, "rebuild": rebuild}
        }),
        "",
    )?);
    assert!(response["error"].is_null(), "{response}");
    assert_ne!(response["result"]["isError"], true, "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["state"], "succeeded",
        "{response}"
    );
    Ok(())
}

fn assert_model(guard: &ServerGuard, root: &Path, model: &str) -> Result<(), Box<dyn Error>> {
    let mut command = Command::new(&guard.binary);
    isolate(&mut command, &guard.home);
    let output = command
        .current_dir(root)
        .args(["--status", "--mode", "server", "--home"])
        .arg(&guard.home)
        .args(["--check-ready", "--color", "never"])
        .output()?;
    assert_command_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("  Embedding   {model}")),
        "{stdout}"
    );
    Ok(())
}

#[test]
fn embedding_policy_matches_direct_server_and_mcp_with_protocol_v2() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let (mut guard, _) = start_server(&binary, &home, "full", None, |command| {
        isolate(command, home.path());
    })?;
    let port = guard.listen.parse::<SocketAddr>()?.port();
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.path().join("daemon/instance.lock"))?)?;
    assert_eq!(record["daemonProtocolVersion"], 2);
    for text in [false, true] {
        let expected = if text { TEXT } else { CODE };
        for mode in ["direct", "server", "mcp"] {
            let root = TempDir::new()?;
            workspace_files(root.path(), text)?;
            if mode == "mcp" {
                mcp_index(port, root.path(), false)?;
            } else {
                cli_index(&guard, root.path(), mode, false)?;
            }
            assert_model(&guard, root.path(), expected)?;
            // Reverse the scene; an omitted model on rebuild must retain the saved choice.
            if text {
                std::fs::remove_file(root.path().join("one.md"))?;
                std::fs::remove_file(root.path().join("two.md"))?;
            } else {
                workspace_files(root.path(), true)?;
            }
            if mode == "mcp" {
                mcp_index(port, root.path(), true)?;
            } else {
                cli_index(&guard, root.path(), mode, true)?;
            }
            assert_model(&guard, root.path(), expected)?;
        }
    }
    let cache = home.path().join("models");
    assert!(!cache.exists() || std::fs::read_dir(cache)?.next().is_none());
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn mcp_first_query_recommends_and_builds_the_workspace_model() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let (mut guard, _) = start_server(&binary, &home, "full", None, |command| {
        isolate(command, home.path());
    })?;
    let port = guard.listen.parse::<SocketAddr>()?.port();
    for text in [false, true] {
        let root = TempDir::new()?;
        workspace_files(root.path(), text)?;
        let expected = if text { TEXT } else { CODE };
        let original =
            json!({"name":"zvec_grep_search", "arguments":{"root":root.path(), "fts":"needle"}});
        let prompt = rpc(&modern_post(port, "tools/call", original.clone(), "")?);
        assert_eq!(prompt["result"]["resultType"], "input_required", "{prompt}");
        let form = &prompt["result"]["inputRequests"]["index_setup"]["params"];
        assert_eq!(
            form["requestedSchema"]["properties"]["model"]["default"], expected,
            "{prompt}"
        );
        assert!(
            form["message"]
                .as_str()
                .expect("message")
                .contains(&format!("Recommended: {expected}"))
        );
        assert!(!root.path().join(".zvec-grep").exists());
        let mut accepted = original.clone();
        accepted["requestState"] = prompt["result"]["requestState"].clone();
        accepted["inputResponses"] =
            json!({"index_setup":{"action":"accept","content":{"model":expected}}});
        let submitted = rpc(&modern_post(port, "tools/call", accepted, "")?);
        assert_eq!(
            submitted["result"]["structuredContent"]["status"], "indexing",
            "{submitted}"
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let result = rpc(&modern_post(port, "tools/call", original.clone(), "")?);
            assert_ne!(result["result"]["isError"], true, "{result}");
            assert_ne!(result["result"]["resultType"], "input_required", "{result}");
            if result["result"]["structuredContent"]["status"] != "indexing" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "MCP index did not finish: {result}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_model(&guard, root.path(), expected)?;
    }
    assert_command_success(&guard.stop()?);
    Ok(())
}
