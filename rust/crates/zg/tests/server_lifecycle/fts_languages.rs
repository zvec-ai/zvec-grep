use super::*;

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

fn mapping(home: &Path, config: &serde_json::Value) -> Result<PathBuf, Box<dyn Error>> {
    let directory = home.join(".zvec-grep");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("fts-languages.json");
    std::fs::write(
        &path,
        json!({"version": 1, "languages": {"shared": config}}).to_string(),
    )?;
    Ok(path)
}

fn analysis(root: &Path) -> Result<serde_json::Value, Box<dyn Error>> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(".zvec-grep/manifest.json"))?)?;
    Ok(manifest["index"]["descriptor"]["fts"].clone())
}

fn cli_index(
    guard: &ServerGuard,
    user: &Path,
    root: &Path,
    mode: &str,
) -> Result<(), Box<dyn Error>> {
    let mut command = Command::new(&guard.binary);
    isolate(&mut command, user);
    let output = command
        .current_dir(root)
        .args(["--index", "--mode", mode, "--home"])
        .arg(&guard.home)
        .args([
            "--fts-lang",
            "shared",
            "--embedding",
            "local/potion-code-16m-v2",
        ])
        .output()?;
    assert_command_success(&output);
    Ok(())
}

fn mcp_index(
    port: u16,
    root: &Path,
    language: Option<&str>,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let mut arguments =
        json!({"root": root, "wait": true, "embedding": "local/potion-code-16m-v2"});
    if let Some(name) = language {
        arguments["ftsLang"] = json!(name);
    }
    Ok(rpc(&modern_post(
        port,
        "tools/call",
        json!({"name": "zvec_grep_index", "arguments": arguments}),
        "",
    )?))
}

fn succeeded(response: &serde_json::Value) {
    assert!(response["error"].is_null(), "{response}");
    assert_ne!(response["result"]["isError"], true, "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["state"], "succeeded",
        "{response}"
    );
}

#[test]
fn fts_language_cli_expands_before_transport_and_mcp_reloads_server_mappings()
-> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let server_home = TempDir::new()?;
    let client_home = TempDir::new()?;
    let client_config = json!({"tokenizer":"whitespace","filters":[],"extraParams":{}});
    let server_config = json!({"tokenizer":"standard","filters":["lowercase","stemmer"],"extraParams":{"stemmer_lang":"russian"}});
    let server_file = mapping(server_home.path(), &server_config)?;
    mapping(client_home.path(), &client_config)?;
    let (mut guard, _) = start_server(&binary, &server_home, "full", None, |command| {
        isolate(command, server_home.path());
    })?;
    let port = guard.listen.parse::<SocketAddr>()?.port();
    for mode in ["direct", "server"] {
        let root = TempDir::new()?;
        cli_index(&guard, client_home.path(), root.path(), mode)?;
        assert_eq!(analysis(root.path())?, client_config, "{mode}");
    }
    let original = TempDir::new()?;
    succeeded(&mcp_index(port, original.path(), Some("shared"))?);
    assert_eq!(analysis(original.path())?, server_config);
    let changed = json!({"tokenizer":"jieba","filters":["lowercase"],"extraParams":{}});
    mapping(server_home.path(), &changed)?;
    let next = TempDir::new()?;
    succeeded(&mcp_index(port, next.path(), Some("shared"))?);
    assert_eq!(analysis(next.path())?, changed);
    succeeded(&mcp_index(port, original.path(), None)?);
    assert_eq!(analysis(original.path())?, server_config);
    std::fs::write(server_file, "invalid JSON")?;
    let root = TempDir::new()?;
    cli_index(&guard, client_home.path(), root.path(), "server")?;
    assert_eq!(analysis(root.path())?, client_config);
    succeeded(&mcp_index(port, original.path(), None)?);
    let invalid = TempDir::new()?;
    let response = mcp_index(port, invalid.path(), Some("shared"))?;
    assert!(
        response.to_string().contains("fts-languages.json"),
        "{response}"
    );
    assert!(!invalid.path().join(".zvec-grep/manifest.json").exists());
    assert_command_success(&guard.stop()?);
    Ok(())
}
