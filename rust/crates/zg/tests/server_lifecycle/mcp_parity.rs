use super::*;

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
    mut params: serde_json::Value,
    extra: &str,
    version: &str,
) -> Result<String, Box<dyn Error>> {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientCapabilities": {"elicitation": {"form": {}}},
        "io.modelcontextprotocol/clientInfo": {"name": "parity-test", "version": "1"}
    });
    let name_header = params["name"]
        .as_str()
        .map_or(String::new(), |name| format!("Mcp-Name: {name}\r\n"));
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    write!(
        stream,
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: {version}\r\nMcp-Method: {method}\r\n{name_header}{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
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
                    if toolset == "agent" { 1 } else { 6 }
                );
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
