use super::*;

fn call_tool(
    bridge: &mut StdioBridge,
    name: &str,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn Error>> {
    static REQUEST_ID: AtomicUsize = AtomicUsize::new(2);
    let response = bridge.request(&json!({
        "jsonrpc": "2.0", "id": REQUEST_ID.fetch_add(1, Ordering::Relaxed), "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    }))?;
    assert_eq!(response["result"]["isError"], false, "{name}: {response}");
    Ok(response["result"]["structuredContent"].clone())
}

fn runtime_count(bridge: &mut StdioBridge) -> Result<u64, Box<dyn Error>> {
    call_tool(bridge, "zvec_grep_server_status", &json!({}))?["active_runtimes"]
        .as_u64()
        .ok_or_else(|| "missing active runtime count".into())
}

#[test]
fn mcp_workspaces_are_ephemeral_and_ignore_legacy_registries() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let sources = TempDir::new()?;
    let roots = [
        sources.path().join("first/docs"),
        sources.path().join("second/docs"),
    ];
    for root in &roots {
        std::fs::create_dir_all(root)?;
    }
    let config_home = home.path().join(".zvec-grep");
    std::fs::create_dir(&config_home)?;
    let registry = config_home.join("workspaces.json");
    let overridden_registry = home.path().join("legacy-workspaces.json");
    let stale = serde_json::to_vec(&json!({
        "version": 1,
        "workspaces": [{"name": "docs", "root": roots[0].canonicalize()?}]
    }))?;

    // Restarts must neither restore runtime entries nor consult legacy files.
    for legacy in [Some(stale), Some(b"invalid legacy JSON".to_vec()), None] {
        for path in [&registry, &overridden_registry] {
            if let Some(bytes) = &legacy {
                std::fs::write(path, bytes)?;
            } else {
                std::fs::remove_file(path)?;
            }
        }
        let (mut guard, _) = start_server(&binary, &home, "full", None, |command| {
            command
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("ZVEC_GREP_WORKSPACE_REGISTRY", &overridden_registry)
                .env("ZVEC_GREP_WATCHER_IDLE_TIMEOUT_SECONDS", "3");
        })?;
        let mut bridge = StdioBridge::spawn(&binary, home.path(), &guard.listen)?;
        bridge.request(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": { "name": "workspace-lifecycle-test", "version": "1" } }
        }))?;
        bridge.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        assert_eq!(runtime_count(&mut bridge)?, 0);
        for root in &roots {
            let result = call_tool(
                &mut bridge,
                "zvec_grep_index",
                &json!({
                    "root": root, "wait": true, "embedding": "local/potion-code-16m-v2"
                }),
            )?;
            assert_eq!(result["state"], "succeeded", "{result}");
        }
        for root in &roots {
            let info = call_tool(
                &mut bridge,
                "zvec_grep_index_status",
                &json!({"root": root}),
            )?;
            let workspace = &info["persistent"]["workspace_index"];
            assert!(workspace.get("name").is_none(), "{info}");
            assert!(workspace.get("id").is_none(), "{info}");
            assert_eq!(
                workspace["root"],
                json!(zg_cli::output_path(&root.canonicalize()?)),
                "{info}"
            );
        }
        assert_eq!(runtime_count(&mut bridge)?, 2);
        let deadline = Instant::now() + Duration::from_secs(15);
        while runtime_count(&mut bridge)? != 0 {
            assert!(
                Instant::now() < deadline,
                "idle workspaces were not retired"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // Local indexes survive retirement and reactivate only their requested root.
        call_tool(
            &mut bridge,
            "zvec_grep_search",
            &json!({
                "root": roots[0], "fts": "absent"
            }),
        )?;
        assert_eq!(runtime_count(&mut bridge)?, 1);
        bridge.close()?;
        assert_command_success(&guard.stop()?);
        for path in [&registry, &overridden_registry] {
            assert_eq!(std::fs::read(path).ok(), legacy);
            assert!(!path.with_extension("json.lock").exists());
        }
    }
    Ok(())
}
