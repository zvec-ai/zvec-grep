use super::*;

const MODEL: &str = "qwen/text-embedding-v4";
const FILES: [&str; 3] = ["guide.md", "settings.json", "source.rs"];

fn tool_call(id: usize, name: &str, arguments: &serde_json::Value) -> serde_json::Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
}

fn check_cli_status(output: &Output) {
    assert_command_success(output);
    let text = String::from_utf8_lossy(&output.stdout);
    for kind in ["text", "code"] {
        assert!(
            text.lines()
                .any(|line| line.split_whitespace().eq([kind, MODEL])),
            "{text}"
        );
    }
}

#[test]
fn content_kind_routes_status_and_search_match_across_direct_daemon_and_mcp()
-> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    let endpoint = format!("http://{}/embeddings", embedding.address);
    for (path, source) in FILES.into_iter().zip([
        "# Guide\ncompassneedle\n",
        r#"{"token":"compassneedle"}"#,
        "fn compassneedle() {}\n",
    ]) {
        std::fs::write(workspace.path().join(path), source)?;
    }
    let configure = |command: &mut Command| {
        command
            .current_dir(workspace.path())
            .env("ZVEC_GREP_HOME", home.path())
            .env(
                "ZVEC_GREP_WORKSPACE_REGISTRY",
                home.path().join("workspaces.json"),
            )
            .env("ZVEC_GREP_API_KEY", "test-key");
    };
    let cli = |mode: &str, args: &[&str]| -> std::io::Result<Output> {
        let mut command = Command::new(&binary);
        configure(&mut command);
        command
            .args(["--mode", mode, "--home"])
            .arg(home.path())
            .args(args)
            .output()
    };
    let set_code = format!("code={MODEL}");
    let set_text = format!("text={MODEL}");
    assert_command_success(&cli(
        "direct",
        &[
            "--index",
            "--allow-remote",
            "--embedding",
            MODEL,
            "--endpoint",
            &endpoint,
            "--embedding",
            &set_code,
        ],
    )?);
    check_cli_status(&cli("direct", &["--status"])?);
    let (mut guard, _) = start_server(&binary, &home, "full", None, configure)?;
    assert_command_success(&cli(
        "server",
        &[
            "--index",
            "--rebuild",
            "--allow-remote",
            "--embedding",
            &set_text,
            "--embedding",
            "code=default",
            "--endpoint",
            &endpoint,
        ],
    )?);
    check_cli_status(&cli("server", &["--status"])?);

    let mut bridge = StdioBridge::spawn(&binary, home.path(), &guard.listen)?;
    bridge.request(
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-11-25", "capabilities":{"elicitation":{"form":{}}},
        "clientInfo":{"name":"content-kind-test","version":"1"}}}),
    )?;
    bridge.notify(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))?;
    rebuild_and_check_mcp_status(&mut bridge, workspace.path(), &endpoint)?;
    for mode in ["direct", "server"] {
        check_cli_status(&cli(mode, &["--status"])?);
    }
    let requests = embedding.requests.load(Ordering::SeqCst);
    assert!(requests > 0);
    check_searches(&cli, workspace.path(), &mut bridge)?;
    assert_eq!(
        embedding.requests.load(Ordering::SeqCst),
        requests,
        "FTS must work without uploading query text"
    );
    bridge.close()?;
    assert_command_success(&guard.stop()?);
    Ok(())
}

fn rebuild_and_check_mcp_status(
    bridge: &mut StdioBridge,
    root: &Path,
    endpoint: &str,
) -> Result<(), Box<dyn Error>> {
    let index = tool_call(
        2,
        "zvec_grep_index",
        &json!({
            "root":root, "embedding": MODEL, "endpoint": endpoint, "wait":true, "rebuild":true,
            "embeddingRoutes":{"text":"default", "code": MODEL}
        }),
    );
    let (result, _) = bridge.request_with_consent(&index, "once")?;
    assert_eq!(
        result["result"]["structuredContent"]["state"], "succeeded",
        "{result}"
    );
    let status = bridge.request(&tool_call(
        3,
        "zvec_grep_index_status",
        &json!({"root":root}),
    ))?;
    let indexes =
        status["result"]["structuredContent"]["persistent"]["workspace_index"]["content_indexes"]
            .as_array()
            .ok_or_else(|| format!("missing content indexes: {status}"))?;
    assert_eq!(indexes.len(), 2);
    for (kind, inherited) in [("text", true), ("code", false)] {
        let route = indexes
            .iter()
            .find(|index| index["kind"] == kind)
            .ok_or("missing route")?;
        assert_eq!(route["inherited"], inherited);
        assert_eq!(route["embedding"]["provider"], "qwen");
        assert_eq!(route["embedding"]["model"], "text-embedding-v4");
    }
    Ok(())
}

fn check_searches(
    cli: &impl Fn(&str, &[&str]) -> std::io::Result<Output>,
    root: &Path,
    bridge: &mut StdioBridge,
) -> Result<(), Box<dyn Error>> {
    let cases: &[(&[&str], Option<&str>, [bool; 3])] = &[
        (&[], None, [true, true, true]),
        (&["text"], None, [true, true, false]),
        (&["code"], None, [false, false, true]),
        (&["text"], Some("data"), [false, true, false]),
        (&["text", "code"], Some("document"), [true, false, false]),
    ];
    for (index, &(kinds, category, expected)) in cases.iter().enumerate() {
        let mut args = vec![
            "--fts",
            "compassneedle",
            "--refresh",
            "off",
            "--preview",
            "short",
        ];
        for kind in kinds {
            args.extend(["--kind", kind]);
        }
        let mut mcp =
            json!({"root":root, "fts":"compassneedle", "autoUpdate":false, "preview":"short"});
        if !kinds.is_empty() {
            mcp["kinds"] = json!(kinds);
        }
        if let Some(category) = category {
            args.extend(["--category", category]);
            mcp["categories"] = json!([category]);
        }
        let mcp = bridge.request(&tool_call(index + 4, "zvec_grep_search", &mcp))?;
        assert_eq!(mcp["result"]["isError"], false, "{mcp}");
        let body = mcp["result"]["content"][0]["text"]
            .as_str()
            .ok_or("missing MCP search output")?;
        for (path, present) in FILES.into_iter().zip(expected) {
            assert_eq!(body.contains(path), present, "{args:?} {body}");
        }
        for mode in ["direct", "server"] {
            let result = cli(mode, &args)?;
            assert_command_success(&result);
            let text = String::from_utf8(result.stdout)?;
            assert!(body.ends_with(&text), "{mode} {args:?}: {text} vs {body}");
        }
    }
    Ok(())
}
