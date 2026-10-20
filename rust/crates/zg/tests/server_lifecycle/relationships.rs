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
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    std::fs::write(
        workspace.path().join("calls.py"),
        "def target():\n    pass\n\ndef caller():\n    target()\n",
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
    // Graph extraction is a separate change. Seed one persisted call against
    // the actual canonical symbols so this test exercises the query chain.
    seed_persisted_call(workspace.path())?;
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
        for mode in ["direct", "server"] {
            let output = Command::new(&binary)
                .arg(format!("--{tool}"))
                .arg(format!(" {symbol} "))
                .arg(workspace.path())
                .args(["--mode", mode, "--home"])
                .arg(home.path())
                .output()?;
            assert_command_success(&output);
            let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            assert_eq!(reply["matches"].as_array().ok_or("matches array")?.len(), 1);
            assert_eq!(reply["suggestions"], json!([]));
            assert_eq!(reply["matches"][0]["name"], symbol);
            assert_eq!(reply["matches"][0]["truncated"], false);
            assert_eq!(reply["matches"][0]["edges"][0]["symbol"]["name"], endpoint);
            assert_eq!(reply["matches"][0]["edges"][0]["line"], 5);
            assert_eq!(reply["matches"][0]["edges"][0]["filePath"], "calls.py");
            replies.push(reply);
        }
        assert_eq!(replies[0], replies[1]);
        let mcp = bridge.request(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": {"root": workspace.path(), "symbol": format!(" {symbol} ")}}
        }))?;
        assert_eq!(mcp["result"]["structuredContent"], replies[0]);
    }
    assert_suggestions(&binary, home.path(), workspace.path(), &mut bridge)?;
    assert_path_filters(&binary, home.path(), workspace.path(), &mut bridge)?;
    bridge.close()?;
    assert_command_success(&guard.stop()?);
    Ok(())
}

fn assert_path_filters(
    binary: &Path,
    home: &Path,
    workspace: &Path,
    bridge: &mut StdioBridge,
) -> Result<(), Box<dyn Error>> {
    let nested = workspace.join("src");
    std::fs::create_dir_all(&nested)?;
    for (id, path, expected) in [(50, "calls.py", 1), (51, "missing.py", 0)] {
        for mode in ["direct", "server"] {
            let output = Command::new(binary)
                .current_dir(workspace)
                .args(["--callers", "target"])
                .arg(workspace)
                .args(["--path", path, "--mode", mode, "--home"])
                .arg(home)
                .output()?;
            assert_command_success(&output);
            let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            assert_eq!(
                reply["matches"].as_array().ok_or("matches")?.len(),
                expected
            );
        }
        let mcp = bridge.request(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "callers", "arguments": {"root": workspace, "symbol": "target", "path": path}}
        }))?;
        assert_eq!(
            mcp["result"]["structuredContent"]["matches"]
                .as_array()
                .ok_or("MCP matches")?
                .len(),
            expected
        );
    }
    for (mode, symbol, path, expected_path, suggestions) in [
        ("direct", "nested_entry", "main.py", "src/main.py", false),
        ("server", "nested_entry", "main.py", "src/main.py", false),
        ("server", "NESTED_ENTRY", "main.py", "src/main.py", true),
        ("direct", "target", "../calls.py", "calls.py", false),
    ] {
        let output = Command::new(binary)
            .current_dir(&nested)
            .args([
                "--callers",
                symbol,
                "--path",
                path,
                "--mode",
                mode,
                "--home",
            ])
            .arg(home)
            .output()?;
        assert_command_success(&output);
        let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let values = reply[if suggestions {
            "suggestions"
        } else {
            "matches"
        }]
        .as_array()
        .ok_or("filtered results")?;
        assert_eq!(values.len(), 1);
        assert_eq!(
            Path::new(values[0]["filePath"].as_str().ok_or("file path")?),
            Path::new(expected_path)
        );
    }
    Ok(())
}

fn assert_suggestions(
    binary: &Path,
    home: &Path,
    workspace: &Path,
    bridge: &mut StdioBridge,
) -> Result<(), Box<dyn Error>> {
    let output = Command::new(binary)
        .args(["--callers", "WrongScope::TARGET"])
        .arg(workspace)
        .args(["--mode", "direct", "--home"])
        .arg(home)
        .output()?;
    assert_command_success(&output);
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(reply["matches"], json!([]));
    assert_eq!(
        reply["suggestions"].as_array().ok_or("suggestions")?.len(),
        1
    );
    assert_eq!(reply["suggestions"][0]["name"], "target");
    let mcp = bridge.request(&json!({
        "jsonrpc": "2.0", "id": 20, "method": "tools/call",
        "params": {"name": "callers", "arguments": {"root": workspace, "symbol": "WrongScope::TARGET"}}
    }))?;
    assert_eq!(mcp["result"]["structuredContent"], reply);
    let text = mcp["result"]["content"][0]["text"].as_str().ok_or("text")?;
    assert_eq!(serde_json::from_str::<serde_json::Value>(text)?, reply);
    Ok(())
}

fn seed_persisted_call(root: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let home = root.join(".zvec-grep");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("manifest.json"))?)?;
    let storage = home.join("generations").join(
        manifest["storageGeneration"]
            .as_str()
            .ok_or("storage generation")?,
    );
    let config = zvec_rust::ConfigBuilder::new()
        .num_threads(2)
        .memory_limit(512 * 1024 * 1024)
        .build();
    zvec_rust::initialize(Some(&config))?;
    let mut options = zvec_rust::CollectionOptions::new()?;
    options.set_read_only(true)?;
    let entities_path = storage.join("storage/entities");
    let entities =
        zvec_rust::Collection::open(entities_path.to_str().ok_or("entity path")?, Some(&options))?;
    let mut query = zvec_rust::SearchQuery::scalar(100)?;
    query.set_output_fields(&["entity_id", "file_id", "metadata"])?;
    let mut symbols = std::collections::HashMap::new();
    for entity in entities.query(&query)? {
        let Some(metadata) = entity.get_string("metadata")? else {
            continue;
        };
        let metadata: serde_json::Value = serde_json::from_str(&metadata)?;
        if let Some(name) = metadata["symbol_name"].as_str() {
            symbols.insert(
                name.to_owned(),
                (
                    entity.get_u32("file_id")?.ok_or("file ID")?,
                    entity.get_string("entity_id")?.ok_or("entity ID")?,
                ),
            );
        }
    }
    let (file_id, caller) = symbols.get("caller").ok_or("caller definition")?;
    let (_, target) = symbols.get("target").ok_or("target definition")?;
    // IndexStore creates the graph database; extraction still supplies no edges.
    // Seed a call into that production schema to exercise the query transports.
    assert!(storage.join("graph/graph.sqlite").is_file());
    let graph = rusqlite::Connection::open(storage.join("graph/graph.sqlite"))?;
    assert_eq!(
        graph.query_row("SELECT count(*) FROM edges", [], |row| row.get::<_, u64>(0))?,
        0
    );
    graph.execute(
        "INSERT INTO edges (file_id, source, target, kind, status, line, col, provenance) VALUES (?, ?, ?, 'calls', 'resolved', 5, 4, 'file_local')",
        rusqlite::params![file_id, caller, target],
    )?;
    Ok(())
}
