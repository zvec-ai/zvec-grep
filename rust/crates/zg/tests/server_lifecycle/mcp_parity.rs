use super::*;

const SNAPSHOT_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0,
    0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0, 0, 3, 1,
    1, 0, 201, 254, 146, 239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];
const SNAPSHOT_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

#[test]
fn multimodal_targets_groups_and_snapshots_survive_direct_server_and_mcp()
-> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    build_multimodal_fixture(&binary, &home, &workspace, embedding.address)?;
    let (mut guard, _) = start_server(&binary, &home, "full", None, |command| {
        configure_multimodal_command(command, &home, &workspace);
    })?;
    let port = guard.listen.parse::<SocketAddr>()?.port();
    let mut image_reference = None;
    for query in [
        &["--vector", "orchard"][..],
        &["--input", "picture.png"][..],
    ] {
        image_reference = Some(check_multimodal_queries(
            &binary, &home, &workspace, port, query,
        )?);
    }
    // Snapshot reads must work after the original image disappears, without refreshing.
    std::fs::remove_file(workspace.path().join("picture.png"))?;
    check_multimodal_snapshot(
        &binary,
        &home,
        &workspace,
        port,
        &image_reference.expect("image result has a reference"),
    )?;
    check_embedding_inheritance(&binary, &home, &workspace, embedding.address)?;
    assert_command_success(&guard.stop()?);
    Ok(())
}

fn check_multimodal_queries(
    binary: &Path,
    home: &TempDir,
    workspace: &TempDir,
    port: u16,
    query: &[&str],
) -> Result<serde_json::Value, Box<dyn Error>> {
    let image_input = query[0] == "--input";
    let mcp_query = if image_input {
        json!({"queryImage": {"source": "path", "path": workspace.path().join(query[1])}})
    } else {
        json!({"vector": query[1]})
    };
    let mut image_reference = None;
    for target in [Some("code"), Some("image"), None] {
        let mut direct_result = None;
        for mode in ["direct", "server"] {
            let mut command = Command::new(binary);
            configure_multimodal_command(&mut command, home, workspace);
            command.args(query).args([
                "--mode",
                mode,
                "--refresh",
                "off",
                "--limit",
                "6",
                "--json",
            ]);
            if let Some(kind) = target {
                command.args(["--kind", kind]);
            }
            let output = command.output()?;
            assert_command_success(&output);
            let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            assert_eq!(
                result["diagnostics"]["index"]["input_kind"],
                if image_input { "image" } else { "text" },
                "CLI and daemon retain the query input kind"
            );
            let items = result["items"].as_array().expect("search items");
            assert!(!items.is_empty(), "{result}");
            if let Some(kind) = target {
                assert!(items.iter().all(|item| item["preview"]["kind"] == kind));
            } else {
                let groups = result["diagnostics"]["index"]["result_groups"]
                    .as_array()
                    .expect("result groups");
                assert_eq!(groups.len(), 3, "unpinned remote spaces remain grouped");
                for kind in ["text", "code", "image"] {
                    assert!(groups.iter().any(|group| group["kinds"] == json!([kind])));
                }
            }
            if mode == "direct" {
                if target == Some("image") {
                    image_reference = Some(items[0]["content_ref"].clone());
                }
                direct_result = Some(result);
            } else {
                let direct = direct_result.as_ref().expect("direct query ran first");
                assert_eq!(search_projection(&result), search_projection(direct));
                assert_mcp_search_parity(
                    port,
                    workspace.path(),
                    mcp_query.clone(),
                    target,
                    direct,
                )?;
            }
        }
    }
    Ok(image_reference.expect("image result has a reference"))
}

fn configure_multimodal_command(command: &mut Command, home: &TempDir, workspace: &TempDir) {
    #[cfg(unix)]
    {
        // Exercise normal macOS shell limits without changing the test process.
        // This also wraps daemon startup; its child inherits the CLI's new limit.
        let mut constrained = Command::new("/bin/sh");
        constrained
            .args([
                "-c",
                "ulimit -S -n 256 || exit; exec \"$@\"",
                "zg-low-nofile",
            ])
            .arg(command.get_program())
            .args(command.get_args());
        *command = constrained;
    }
    command
        .current_dir(workspace.path())
        .env("ZVEC_GREP_HOME", home.path())
        .env(
            "ZVEC_GREP_WORKSPACE_REGISTRY",
            home.path().join("workspaces.json"),
        )
        .env(
            "ZVEC_GREP_AUTHORIZATION_KEY_FILE",
            home.path().join("authorization.key"),
        )
        .env("ZVEC_GREP_API_KEY", "local-test-key");
}

fn build_multimodal_fixture(
    binary: &Path,
    home: &TempDir,
    workspace: &TempDir,
    address: SocketAddr,
) -> Result<(), Box<dyn Error>> {
    std::fs::write(workspace.path().join("notes.txt"), "orchard notes\n")?;
    std::fs::write(
        workspace.path().join("source.rs"),
        "pub fn orchard() -> &'static str { \"fruit\" }\n",
    )?;
    std::fs::write(workspace.path().join("picture.png"), SNAPSHOT_PNG)?;
    let endpoint = format!("http://{address}/embeddings");
    let mut command = Command::new(binary);
    configure_multimodal_command(&mut command, home, workspace);
    let output = command
        .args([
            "--auth",
            "grant",
            path_text(workspace.path())?,
            "--capability",
            "embedding",
            "--scope",
            "workspace",
            "--embedding",
            "qwen/qwen3-vl-embedding",
            "--endpoint",
            &endpoint,
        ])
        .output()?;
    assert_command_success(&output);
    let mut command = Command::new(binary);
    configure_multimodal_command(&mut command, home, workspace);
    let output = command
        .args([
            "--index",
            "--mode",
            "direct",
            "--embedding",
            "qwen/qwen3-vl-embedding",
            "--embedding",
            "image=qwen/qwen3-vl-embedding",
            "--endpoint",
            &endpoint,
        ])
        .output()?;
    assert_command_success(&output);
    Ok(())
}

fn check_embedding_inheritance(
    binary: &Path,
    home: &TempDir,
    workspace: &TempDir,
    address: SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let read_status = |mode: &str| -> Result<serde_json::Value, Box<dyn Error>> {
        let mut command = Command::new(binary);
        configure_multimodal_command(&mut command, home, workspace);
        let output = command
            .args(["--status", "--json", "--mode", mode])
            .output()?;
        assert_command_success(&output);
        Ok(serde_json::from_slice(&output.stdout)?)
    };
    let initial = read_status("server")?;
    assert_eq!(
        initial["workspace_index"]["embedding_routes"],
        json!({"image": "qwen/qwen3-vl-embedding"})
    );
    let endpoint = format!("http://{address}/embeddings");
    for (model, clear_image, kinds) in [
        ("qwen/text-embedding-v4", true, json!(["text", "code"])),
        (
            "qwen/qwen3-vl-embedding",
            false,
            json!(["text", "code", "image"]),
        ),
    ] {
        let mut command = Command::new(binary);
        configure_multimodal_command(&mut command, home, workspace);
        command.args([
            "--index",
            "--rebuild",
            "--mode",
            "server",
            "--embedding",
            model,
            "--endpoint",
            &endpoint,
            "--allow-remote",
        ]);
        if clear_image {
            command.args(["--embedding", "image=default"]);
        }
        assert_command_success(&command.output()?);
        for mode in ["direct", "server"] {
            let status = read_status(mode)?;
            let index = &status["workspace_index"];
            assert_eq!(index["default_model_ref"], model);
            assert_eq!(index["embedding_routes"], json!({}));
            let actual_kinds = index["tables"]
                .as_array()
                .expect("index tables")
                .iter()
                .map(|table| table["kind"].clone())
                .collect::<Vec<_>>();
            assert_eq!(
                json!(actual_kinds),
                kinds,
                "image=default keeps inheriting later default model changes"
            );
        }
    }
    Ok(())
}

fn search_projection(result: &serde_json::Value) -> serde_json::Value {
    let index = &result["diagnostics"]["index"];
    let items = result["items"]
        .as_array()
        .expect("search items")
        .iter()
        .map(|item| {
            json!({
                "rank": item["rank"], "path": item["relative_path"], "preview": item["preview"],
                "reference": item["content_ref"], "score": item["score"],
            })
        })
        .collect::<Vec<_>>();
    json!({
        "items": items, "groups": index["result_groups"], "targets": index["targets"],
        "input_kind": index["input_kind"], "incomplete": index["incomplete"],
    })
}

fn assert_mcp_search_parity(
    port: u16,
    root: &Path,
    mut arguments: serde_json::Value,
    target: Option<&str>,
    expected: &serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    arguments["root"] = json!(root);
    arguments["autoUpdate"] = json!(false);
    arguments["limit"] = json!(6);
    if let Some(target) = target {
        arguments["targetKind"] = json!(target);
    }
    let response = rpc(&modern_post(
        port,
        "tools/call",
        json!({
            "name": "zvec_grep_search", "arguments": arguments,
        }),
        "",
    )?);
    assert!(response["error"].is_null(), "{response}");
    assert_ne!(response["result"]["isError"], true, "{response}");
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("MCP search text");
    let items = expected["items"].as_array().expect("search items");
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("type: "))
            .count(),
        items.len(),
        "{text}"
    );
    let mut remaining = text;
    for item in items {
        let path = item["relative_path"].as_str().expect("result path");
        let position = remaining
            .find(path)
            .unwrap_or_else(|| panic!("missing ordered path {path}: {text}"));
        remaining = &remaining[position + path.len()..];
        assert!(remaining.contains(&format!(
            "type: {}",
            item["preview"]["kind"].as_str().expect("kind")
        )));
        let reference = json!({
            "generation": item["content_ref"]["generation"],
            "entityId": item["content_ref"]["entity_id"],
        });
        assert!(
            remaining.contains(&format!("reference: {reference}")),
            "{text}"
        );
    }
    for group in expected["diagnostics"]["index"]["result_groups"]
        .as_array()
        .expect("groups")
    {
        let id = group["id"].as_str().expect("group id");
        let kinds = group["kinds"]
            .as_array()
            .expect("group kinds")
            .iter()
            .map(|kind| kind.as_str().expect("kind"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            text.contains(&format!("result group: {id} kinds={kinds} ")),
            "{text}"
        );
    }
    Ok(())
}

fn check_multimodal_snapshot(
    binary: &Path,
    home: &TempDir,
    workspace: &TempDir,
    port: u16,
    reference: &serde_json::Value,
) -> Result<(), Box<dyn Error>> {
    for mode in ["direct", "server"] {
        let destination = home.path().join(format!("snapshot-{mode}.png"));
        let mut command = Command::new(binary);
        configure_multimodal_command(&mut command, home, workspace);
        let output = command
            .args([
                "--mode",
                mode,
                "--read-content",
                reference["entity_id"].as_str().expect("entity id"),
                "--generation",
                reference["generation"].as_str().expect("generation"),
                "--output",
                path_text(&destination)?,
            ])
            .output()?;
        assert_command_success(&output);
        assert_eq!(std::fs::read(destination)?, SNAPSHOT_PNG);
    }
    let response = rpc(&modern_post(
        port,
        "tools/call",
        json!({
            "name": "zvec_grep_read_content", "arguments": {"root": workspace.path(), "reference": {
                "generation": reference["generation"], "entityId": reference["entity_id"],
            }},
        }),
        "",
    )?);
    assert!(response["error"].is_null(), "{response}");
    assert_ne!(response["result"]["isError"], true, "{response}");
    let image = response["result"]["content"]
        .as_array()
        .expect("MCP content")
        .iter()
        .find(|block| block["type"] == "image")
        .expect("indexed image block");
    assert_eq!(image["mimeType"], "image/png");
    assert_eq!(image["data"], SNAPSHOT_PNG_BASE64);
    Ok(())
}

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

pub(super) fn rpc(response: &str) -> serde_json::Value {
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
                    if toolset == "agent" { 2 } else { 7 }
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
