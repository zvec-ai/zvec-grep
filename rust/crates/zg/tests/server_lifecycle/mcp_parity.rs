use super::*;

#[path = "embedding_policy.rs"]
mod embedding_policy;

fn modern_post(
    port: u16,
    method: &str,
    params: serde_json::Value,
    extra: &str,
) -> Result<String, Box<dyn Error>> {
    protocol_post(port, method, params, extra, "2026-07-28")
}

fn protocol_post(
    port: u16,
    method: &str,
    params: serde_json::Value,
    extra: &str,
    version: &str,
) -> Result<String, Box<dyn Error>> {
    post_as_client(port, method, params, extra, version, "parity-test")
}

fn post_as_client(
    port: u16,
    method: &str,
    mut params: serde_json::Value,
    extra: &str,
    version: &str,
    client: &str,
) -> Result<String, Box<dyn Error>> {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientCapabilities": {"elicitation": {"form": {}}},
        "io.modelcontextprotocol/clientInfo": {"name": client, "version": "1"}
    });
    let name_header = params["name"]
        .as_str()
        .map_or(String::new(), |name| format!("Mcp-Name: {name}\r\n"));
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: {version}\r\nMcp-Method: {method}\r\n{name_header}{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    Ok(read_http_response(&mut stream)?)
}

fn rpc(response: &str) -> serde_json::Value {
    response
        .lines()
        .find_map(|line| {
            serde_json::from_str::<serde_json::Value>(line.strip_prefix("data: ").unwrap_or(line))
                .ok()
                .filter(|value| value.get("jsonrpc").is_some())
        })
        .unwrap_or_else(|| panic!("missing JSON-RPC response: {response}"))
}

#[test]
fn claude_receives_compact_instructions_on_http_and_stdio() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let (mut guard, _) = start_server(&binary, &home, "full", None, |_| {})?;
    let port = guard.listen.parse::<SocketAddr>()?.port();
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("../../../../compat/mcp/toolsets.json"))?;
    let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}});
    let mut claude_instructions = None;

    for (name, version) in [("claude-code", "2.1.284"), ("other-client", "1.0.0")] {
        let initialize = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": { "name": name, "version": version }
            }
        });
        let modern = rpc(&post_as_client(
            port,
            "server/discover",
            json!({}),
            "",
            "2026-07-28",
            name,
        )?);
        let http = rpc(&post_json(port, None, &initialize.to_string())?);
        let mut stdio = StdioBridge::spawn(&binary, home.path(), &guard.listen)?;
        let stdio_init = stdio.request(&initialize)?;
        let instructions = modern["result"]["instructions"]
            .as_str()
            .ok_or("discovery instructions")?;
        assert_eq!(http["result"]["instructions"], instructions);
        assert_eq!(stdio_init["result"]["instructions"], instructions);
        assert_eq!(http["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(stdio_init["result"]["protocolVersion"], "2025-11-25");
        if name == "claude-code" {
            assert!(instructions.encode_utf16().count() <= 2_048);
            assert!(instructions.contains("Use zvec_grep_rg for exact-only lookups"));
            assert!(instructions.contains("Require explicit user authorization"));
            claude_instructions = Some(instructions.to_owned());
        } else {
            assert_eq!(baseline["full"]["instructions"], instructions);
        }

        stdio.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        let stdio_tools = stdio.request(&list)?;
        let modern_tools = rpc(&post_as_client(
            port,
            "tools/list",
            json!({}),
            "",
            "2026-07-28",
            name,
        )?);
        assert_eq!(
            stdio_tools["result"]["tools"],
            modern_tools["result"]["tools"]
        );
        let search = modern_tools["result"]["tools"]
            .as_array()
            .ok_or("tool list")?
            .iter()
            .find(|tool| tool["name"] == "zvec_grep_search")
            .ok_or("search tool")?;
        assert_eq!(
            search["description"],
            baseline["full"]["search_description"]
        );
        stdio.close()?;
    }

    let claude = rpc(&post_as_client(
        port,
        "server/discover",
        json!({}),
        "",
        "2026-07-28",
        "claude-code",
    )?);
    assert_eq!(
        claude["result"]["instructions"].as_str(),
        claude_instructions.as_deref()
    );
    assert_command_success(&guard.stop()?);
    Ok(())
}

#[test]
fn modern_http_discovery_tools_errors_and_remote_continuation() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    for toolset in ["agent", "full"] {
        let home = TempDir::new()?;
        let workspace = TempDir::new()?;
        let embedding = EmbeddingServer::start()?;
        std::fs::write(
            workspace.path().join("sample.md"),
            "# Sample\nconsent fixture\n",
        )?;
        let (mut guard, _) = start_server(&binary, &home, toolset, None, |command| {
            command.env("ZVEC_GREP_API_KEY", "test-key");
        })?;
        let port = guard.listen.parse::<SocketAddr>()?.port();
        for method in ["server/discover", "tools/list"] {
            let response = modern_post(port, method, json!({}), "")?;
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            assert!(
                !response.to_lowercase().contains("mcp-session-id:"),
                "modern requests are stateless"
            );
            let result = rpc(&response);
            assert_eq!(result["result"]["ttlMs"], 3_600_000, "{result}");
            assert_eq!(result["result"]["cacheScope"], "private", "{result}");
            if method == "tools/list" {
                assert_eq!(
                    result["result"]["tools"]
                        .as_array()
                        .expect("valid test fixture")
                        .len(),
                    if toolset == "agent" { 3 } else { 8 }
                );
                let search = result["result"]["tools"]
                    .as_array()
                    .expect("tools")
                    .iter()
                    .find(|tool| tool["name"] == "zvec_grep_search")
                    .expect("search tool");
                let schema = &search["inputSchema"];
                assert_eq!(schema["properties"]["limit"]["type"], "integer");
                assert_eq!(schema["properties"]["fuse"]["type"], "boolean");
                let required = schema["required"].as_array().expect("required root");
                assert!(!required.contains(&json!("limit")));
                assert!(!required.contains(&json!("fuse")));
            }
        }
        let forbidden = modern_post(
            port,
            "tools/list",
            json!({}),
            "Origin: https://evil.example\r\n",
        )?;
        assert!(forbidden.starts_with("HTTP/1.1 403"), "{forbidden}");
        let invalid = rpc(&modern_post(
            port,
            "tools/call",
            json!({"name":"zvec_grep_search", "arguments":{"root":workspace.path(), "query":"x", "limit":0}}),
            "",
        )?);
        assert_eq!(invalid["error"]["code"], -32602, "{invalid}");
        check_search_scalar_types(port, workspace.path())?;
        check_protocol_versions(port)?;
        if toolset == "full" {
            let arguments = json!({"root": workspace.path(), "embedding":"qwen/text-embedding-v4", "endpoint":format!("http://{}/embeddings", embedding.address), "wait":true});
            let params = json!({"name":"zvec_grep_index", "arguments":arguments});
            let first = rpc(&modern_post(port, "tools/call", params.clone(), "")?);
            assert_eq!(first["result"]["resultType"], "input_required", "{first}");
            assert_eq!(embedding.requests.load(Ordering::SeqCst), 0);
            let mut retry = params.clone();
            retry["requestState"] = first["result"]["requestState"].clone();
            retry["inputResponses"] = json!({"remote_embedding_authorization":{"action":"accept", "content":{"decision":"allow_once"}}});
            let complete = rpc(&modern_post(port, "tools/call", retry.clone(), "")?);
            assert_eq!(
                complete["result"]["structuredContent"]["state"], "succeeded",
                "{complete}"
            );
            let count = embedding.requests.load(Ordering::SeqCst);
            assert!(count > 0);
            let replay = rpc(&modern_post(port, "tools/call", retry, "")?);
            assert_eq!(replay["error"]["code"], -32602, "{replay}");
            assert_eq!(embedding.requests.load(Ordering::SeqCst), count);
            let search = json!({"name":"zvec_grep_search", "arguments":{"root":workspace.path(), "query":"consent", "autoUpdate":false}});
            let first = rpc(&modern_post(port, "tools/call", search.clone(), "")?);
            let mut local = search;
            local["requestState"] = first["result"]["requestState"].clone();
            local["inputResponses"] = json!({"remote_embedding_authorization":{"action":"accept", "content":{"decision":"use_local_search"}}});
            let result = rpc(&modern_post(port, "tools/call", local, "")?);
            assert_ne!(result["result"]["isError"], true, "{result}");
            assert!(result["error"].is_null(), "{result}");
            assert_eq!(embedding.requests.load(Ordering::SeqCst), count);
        }
        assert_command_success(&guard.stop()?);
    }
    Ok(())
}

#[test]
fn modern_http_rg_files_matches_direct_listing_without_an_index() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    std::fs::create_dir(workspace.path().join("src"))?;
    for index in 0..24 {
        std::fs::write(
            workspace.path().join(format!("src/file-{index:02}.rs")),
            b"\0\xff",
        )?;
    }
    for name in [
        "space name.rs",
        "中文.rs",
        "--files",
        ".hidden.rs",
        "ignored.rs",
    ] {
        std::fs::write(workspace.path().join(name), b"\0\xff")?;
    }
    std::fs::write(workspace.path().join(".ignore"), "ignored.rs\n")?;
    let (mut guard, _) = start_server(&binary, &home, "full", None, |_| {})?;
    let port = guard.listen.parse::<SocketAddr>()?.port();
    let cases: &[&[&str]] = &[
        &[],
        &["src", "-g", "*.rs", "-g", "!file-00.rs"],
        &["-t", "rust", "--max-depth", "1"],
        &["-uu", "-t", "rust"],
        &["--null"],
        &["-0", "-g", "*.rs"],
        &["-g", "missing-*"],
        &["--", "--files"],
        &["missing", "src/file-00.rs"],
        &["missing"],
        &["-e", "[", "-f", "missing-patterns", "src/file-00.rs"],
    ];
    for args in cases {
        let direct = Command::new(&binary)
            .current_dir(workspace.path())
            .args(["--mode", "direct", "--rg", "--files"])
            .args(*args)
            .env("ZVEC_GREP_HOME", home.path())
            .output()?;
        let command = std::iter::once("rg --files".to_owned())
            .chain(args.iter().map(|arg| format!("'{arg}'")))
            .collect::<Vec<_>>()
            .join(" ");
        let reply = rpc(&modern_post(
            port,
            "tools/call",
            json!({"name": "zvec_grep_rg", "arguments": {"root": workspace.path(), "command": command}}),
            "",
        )?);
        assert!(reply["error"].is_null(), "{command}: {reply}");
        assert_eq!(
            reply["result"]["content"][0]["text"],
            String::from_utf8(direct.stdout)?,
            "{command}: {reply}"
        );
        let failed = direct.status.code() == Some(2);
        assert_eq!(reply["result"]["isError"] == true, failed, "{reply}");
        if failed {
            assert!(
                reply["result"]["content"][1]["text"]
                    .as_str()
                    .expect("traversal error")
                    .contains("missing")
            );
        }
    }
    check_rg_file_bounds(port, workspace.path())?;
    assert!(!workspace.path().join(".zvec-grep").exists());
    assert!(!home.path().join("models").exists());
    assert_command_success(&guard.stop()?);
    Ok(())
}

fn check_rg_file_bounds(port: u16, workspace: &Path) -> Result<(), Box<dyn Error>> {
    for (command, expected, truncated) in [
        ("rg --files src | head -2", 2, true),
        ("rg --files src | head -24", 24, false),
    ] {
        let reply = rpc(&modern_post(
            port,
            "tools/call",
            json!({"name": "zvec_grep_rg", "arguments": {"root": workspace, "command": command}}),
            "",
        )?);
        assert!(reply["error"].is_null(), "{reply}");
        assert_ne!(reply["result"]["isError"], true, "{reply}");
        let content = reply["result"]["content"].as_array().expect("text output");
        let paths = content[0]["text"].as_str().expect("file paths");
        assert_eq!(paths.lines().count(), expected);
        let expected_paths = (0..expected)
            .map(|index| PathBuf::from(format!("src/file-{index:02}.rs")))
            .collect::<Vec<_>>();
        assert_eq!(
            paths.lines().map(PathBuf::from).collect::<Vec<_>>(),
            expected_paths
        );
        assert_eq!(content.len(), if truncated { 2 } else { 1 });
        if truncated {
            assert!(
                content[1]["text"]
                    .as_str()
                    .expect("notice")
                    .contains("More files were omitted")
            );
        }
    }
    Ok(())
}

#[test]
fn modern_http_missing_index_requires_selection_and_survives_request_completion()
-> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let (mut guard, _) = start_server(&binary, &home, "agent", None, |_| {})?;
    let port = guard.listen.parse::<SocketAddr>()?.port();
    let original = json!({"name": "zvec_grep_search", "arguments": {"root": workspace.path(), "fts": "needle"}});
    let selection = rpc(&modern_post(port, "tools/call", original.clone(), "")?);
    assert_eq!(
        selection["result"]["resultType"], "input_required",
        "{selection}"
    );
    assert!(!workspace.path().join(".zvec-grep").exists());
    let mut accepted = original.clone();
    accepted["requestState"] = selection["result"]["requestState"].clone();
    accepted["inputResponses"] = json!({"index_setup": {"action": "accept", "content": {"model": "local/potion-code-16m-v2"}}});
    let submitted = rpc(&modern_post(port, "tools/call", accepted, "")?);
    assert_eq!(
        submitted["result"]["structuredContent"]["status"], "indexing",
        "{submitted}"
    );
    assert!(submitted["result"]["structuredContent"]["job_id"].is_string());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let result = rpc(&modern_post(port, "tools/call", original.clone(), "")?);
        assert_ne!(result["result"]["isError"], true, "{result}");
        assert_ne!(
            result["result"]["resultType"], "input_required",
            "model selection must not repeat: {result}"
        );
        if result["result"]["structuredContent"]["status"] != "indexing" {
            assert!(workspace.path().join(".zvec-grep/manifest.json").is_file());
            break;
        }
        assert!(Instant::now() < deadline, "background build did not finish");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !home.path().join("models").exists(),
        "empty workspaces must not download a model"
    );
    assert_command_success(&guard.stop()?);
    Ok(())
}

fn check_search_scalar_types(port: u16, root: &Path) -> Result<(), Box<dyn Error>> {
    for patch in [json!({"limit": "15"}), json!({"fuse": "true"})] {
        let mut arguments = json!({"root": root, "query": "x"});
        arguments
            .as_object_mut()
            .expect("arguments")
            .extend(patch.as_object().expect("patch").clone());
        let invalid = rpc(&modern_post(
            port,
            "tools/call",
            json!({"name": "zvec_grep_search", "arguments": arguments}),
            "",
        )?);
        assert_eq!(invalid["error"]["code"], -32602, "{invalid}");
    }
    Ok(())
}

fn check_protocol_versions(port: u16) -> Result<(), Box<dyn Error>> {
    let future = protocol_post(port, "tools/list", json!({}), "", "2099-01-01")?;
    assert!(future.starts_with("HTTP/1.1 400"), "{future}");
    let expired = post_json(
        port,
        Some("nonexistent-session"),
        &json!({"jsonrpc":"2.0", "id":1, "method":"tools/list", "params":{}}).to_string(),
    )?;
    assert!(expired.starts_with("HTTP/1.1 404"), "{expired}");
    for version in ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"] {
        let initialized = post_json(
            port,
            None,
            &json!({
                "jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
                    "protocolVersion":version, "capabilities":{},
                    "clientInfo":{"name":"legacy-parity", "version":"1"}
                }
            })
            .to_string(),
        )?;
        assert_eq!(
            rpc(&initialized)["result"]["protocolVersion"],
            version,
            "{initialized}"
        );
        assert!(
            initialized.to_lowercase().contains("mcp-session-id:"),
            "{initialized}"
        );
    }
    Ok(())
}
