use super::*;
use zg_daemon_protocol::{DaemonCommand, DaemonReply, ExecutionResult};
use zg_engine::api::context::{
    ContextOptions,
    options::{ContextRoute, ContextRouteMode, RefreshPolicy},
};

fn fixtures() -> Vec<(&'static str, String, String, String)> {
    // Each side exceeds the model's ~15K-character budget, placing the matching
    // fragment strictly inside the entity. Verify that precondition after indexing.
    let paragraph = "The indexed entity keeps its original context even when retrieval splits it into overlapping fragments.\n\n";
    let statement = "    observe('The indexed function keeps its context across overlapping retrieval fragments.');\n";
    vec![
        (
            "long.md",
            "# Neighbor before\n\nexcludedbefore\n\n".into(),
            format!(
                "# Complete entity\n\nentitybegin\n\n{}quasarneedle\n\n{}entityend\n\n",
                paragraph.repeat(200),
                paragraph.repeat(200),
            ),
            "# Neighbor after\n\nexcludedafter\n".into(),
        ),
        (
            "long.ts",
            "function before() { return 'excludedbefore'; }\n\n".into(),
            format!(
                "function entitybegin() {{\n{}    const needle = 'quasarneedle';\n{}    return 'entityend';\n}}",
                statement.repeat(200),
                statement.repeat(200),
            ),
            "\n\nfunction after() { return 'excludedafter'; }\n".into(),
        ),
    ]
}

#[test]
fn full_preview_returns_complete_markdown_and_code_entities_across_transports()
-> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    for (path, prefix, entity, suffix) in fixtures() {
        std::fs::write(
            workspace.path().join(path),
            format!("{prefix}{entity}{suffix}"),
        )?;
    }
    let configure = |command: &mut Command| {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ZVEC_GREP_") {
                command.env_remove(key);
            }
        }
        command
            .current_dir(workspace.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("ZVEC_GREP_HOME", home.path().join("runtime"))
            .env(
                "ZVEC_GREP_WORKSPACE_REGISTRY",
                home.path().join("workspaces.json"),
            )
            .env("ZVEC_GREP_API_KEY", "local-test-key");
    };
    let run = |args: &[&str]| -> std::io::Result<Output> {
        let mut command = Command::new(&binary);
        configure(&mut command);
        command.args(args).output()
    };
    assert_command_success(&run(&[
        "--index",
        "--mode",
        "direct",
        "--allow-remote",
        "--embedding",
        "qwen/text-embedding-v4",
        "--endpoint",
        &format!("http://{}/embeddings", embedding.address),
    ])?);
    // Search must use stored entity content, not reread the source file to expand it.
    for (path, _, _, _) in fixtures() {
        std::fs::write(workspace.path().join(path), "changedondisk\n")?;
    }
    let (guard, _) = start_server(&binary, &home, "agent", None, configure)?;
    verify_fragmented_entities(&guard.listen, workspace.path())?;
    let mut bridge = StdioBridge::spawn_with_toolset(&binary, home.path(), &guard.listen, None)?;
    bridge.request(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": { "name": "full-entity-preview-test", "version": "1" } }
    }))?;
    bridge.notify(
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} }),
    )?;
    verify_previews(&run, home.path(), workspace.path(), &mut bridge)?;
    bridge.close()?;
    Ok(())
}

fn verify_fragmented_entities(listen: &str, root: &Path) -> Result<(), Box<dyn Error>> {
    let command = DaemonCommand::Context(ContextOptions {
        root: Some(root.to_owned()),
        routes: vec![ContextRoute {
            mode: ContextRouteMode::Fts,
            query: "quasarneedle".into(),
        }],
        limit: Some(2),
        auto_update: false,
        refresh: Some(RefreshPolicy::Off),
        ..ContextOptions::default()
    });
    let body = serde_json::to_string(&command)?;
    let mut stream = TcpStream::connect(listen)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "POST /admin/execute HTTP/1.1\r\nHost: {listen}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let response = read_http_response(&mut stream)?;
    let (headers, body) = response.split_once("\r\n\r\n").ok_or("missing body")?;
    assert!(headers.starts_with("HTTP/1.1 200"), "{response}");
    let ExecutionResult::Success(DaemonReply::Context(result)) = serde_json::from_str(body)? else {
        return Err(format!("unexpected context reply: {body}").into());
    };
    assert_eq!(result.items.len(), 2);
    for (path, prefix, entity, _) in fixtures() {
        let item = result
            .items
            .iter()
            .find(|item| item.relative_path == Path::new(path))
            .ok_or("missing fixture result")?;
        let excerpt = item
            .excerpt_range
            .as_ref()
            .ok_or("fixture was not chunked")?;
        assert!(excerpt.start_line() > item.range.start_line(), "{path}");
        assert!(excerpt.last_line() < item.range.last_line(), "{path}");
        assert_eq!(item.content.trim_end(), entity.trim_end(), "{path}");
        assert_eq!(
            item.content_range.start_line(),
            Some(prefix.lines().count() + 1)
        );
        assert_eq!(
            item.content_range.line_count(),
            Some(item.content.lines().count())
        );
    }
    Ok(())
}

fn verify_previews(
    run: &impl Fn(&[&str]) -> std::io::Result<Output>,
    home: &Path,
    root: &Path,
    bridge: &mut StdioBridge,
) -> Result<(), Box<dyn Error>> {
    for (path, prefix, entity, _) in fixtures() {
        for preview in ["none", "short", "full", "detail"] {
            let mut outputs = Vec::new();
            for mode in ["direct", "server"] {
                let output = run(&[
                    "--mode",
                    mode,
                    "--home",
                    path_text(home)?,
                    "--fts",
                    "quasarneedle",
                    "--glob",
                    path,
                    "--limit",
                    "1",
                    "--preview",
                    preview,
                    "--refresh",
                    "off",
                    "--color",
                    "never",
                ])?;
                assert_command_success(&output);
                let text = String::from_utf8(output.stdout)?;
                assert!(text.contains(path), "{mode}/{preview}: {text}");
                for excluded in ["excludedbefore", "excludedafter", "changedondisk"] {
                    assert!(!text.contains(excluded), "{mode}/{preview}: {text}");
                }
                if matches!(preview, "full" | "detail") {
                    assert!(text.contains("quasarneedle"), "{text}");
                    assert!(text.contains("entitybegin"), "{text}");
                    assert!(text.contains("entityend"), "{text}");
                } else {
                    assert!(!text.contains("entityend"), "{preview}: {text}");
                    assert!(text.lines().count() < 25, "bounded preview: {text}");
                }
                if preview == "full" {
                    let first = prefix.lines().count() + 1;
                    for (offset, line) in entity.lines().enumerate() {
                        if !line.is_empty() {
                            assert!(
                                text.contains(&format!("  {}: {line}\n", first + offset)),
                                "{text}"
                            );
                        }
                    }
                    assert!(text.contains("#1 ") && !text.contains("#2 "), "{text}");
                }
                outputs.push(text);
            }
            assert_eq!(outputs[0], outputs[1], "{path}/{preview}");
            if matches!(preview, "short" | "full") {
                let reply = bridge.request(&json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": { "name": "zvec_grep_search", "arguments": {
                        "root": root, "fts": "quasarneedle", "globs": [path],
                        "limit": 1, "autoUpdate": false, "preview": preview
                    } }
                }))?;
                assert_eq!(reply["result"]["isError"], false, "{reply}");
                let text = reply["result"]["content"][0]["text"]
                    .as_str()
                    .ok_or("missing MCP preview")?;
                // Runtime refresh counters may differ; the stored source preview must not.
                assert!(text.starts_with("freshness: possibly_stale\n"), "{text}");
                let (_, body) = text.split_once("\n\n").ok_or("missing diagnostics")?;
                assert_eq!(body, outputs[0], "direct/server/MCP {path}/{preview}");
            }
        }
    }
    Ok(())
}
