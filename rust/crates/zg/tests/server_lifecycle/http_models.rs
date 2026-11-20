use super::*;

struct HttpModelServer {
    server: EmbeddingServer,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl HttpModelServer {
    fn start(bearer: bool, single_input: bool) -> std::io::Result<Self> {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let server = EmbeddingServer::start_with_handler(move |mut stream, _| {
            let (headers, request) = read_embedding_request(&stream)?;
            assert!(headers.starts_with("POST /v1/embeddings HTTP/1.1\r\n"));
            let authorization = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.trim());
            assert_eq!(authorization, bearer.then_some("Bearer fixture-secret"));
            assert!(request.get("dimensions").is_none());
            assert!(request.get("encoding_format").is_none());
            let inputs = if single_input {
                vec![request["input"].as_str().expect("single input")]
            } else {
                request["input"]
                    .as_array()
                    .expect("batched input")
                    .iter()
                    .map(|text| text.as_str().expect("input text"))
                    .collect::<Vec<_>>()
            };
            assert!(!inputs.is_empty());
            let data = inputs
                .iter()
                .enumerate()
                .rev()
                .map(|(index, text)| {
                    assert!(text.starts_with("document: ") || text.starts_with("query: "));
                    json!({"index": index, "embedding": [1.0, 0.0, 0.0, 0.0]})
                })
                .collect::<Vec<_>>();
            captured.lock().expect("requests").push(request);
            write_embedding_response(&mut stream, &json!({"data": data}))
        })?;
        Ok(Self { server, requests })
    }

    fn request_count(&self) -> usize {
        self.server.requests.load(Ordering::SeqCst)
    }
}

fn configure_command(command: &mut Command, home: &Path, workspace: &Path) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ZVEC_GREP_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(workspace)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ZVEC_GREP_HOME", home)
        .env("ZVEC_GREP_API_KEY", "must-not-be-forwarded")
        .env_remove("ZG_HTTP_FIXTURE_KEY");
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Verify CLI, daemon, and MCP against the same persisted model definition"
)]
fn configured_http_models_survive_config_removal_and_daemon_restart() -> Result<(), Box<dyn Error>>
{
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = HttpModelServer::start(false, false)?;
    std::fs::write(
        workspace.path().join("sample.md"),
        "# Authentication\n\nA token authenticates each request.\n",
    )?;
    let config_dir = home.path().join(".zvec-grep");
    std::fs::create_dir_all(&config_dir)?;
    let config_path = config_dir.join("config.json");
    let config = json!({"version": 1, "models": {"http/fixture": {
        "endpoint": format!("http://{}/v1", embedding.server.address),
        "http": {"model": "uncataloged-fixture-model", "dimension": 4,
            "maxInputTokens": 128, "maxBatchSize": 2, "auth": "none",
            "queryPrefix": "query: ", "documentPrefix": "document: "}
    }}});
    std::fs::write(&config_path, serde_json::to_vec(&config)?)?;
    let configure = |command: &mut Command| {
        configure_command(command, home.path(), workspace.path());
    };
    let run = |args: &[&str]| -> std::io::Result<Output> {
        let mut command = Command::new(&binary);
        configure(&mut command);
        command.args(args).output()
    };
    assert_command_success(&run(&[
        "--config",
        "model",
        "set",
        "http/fixture",
        "--default",
    ])?);
    let denied = run(&["--index", "--mode", "direct"])?;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("authorization required"));
    assert_eq!(embedding.request_count(), 0);
    assert_command_success(&run(&[
        "--auth",
        "grant",
        "--capability",
        "embedding",
        "--scope",
        "workspace",
        "--embedding",
        "http/fixture",
    ])?);
    assert_command_success(&run(&[
        "--index",
        "--mode",
        "direct",
        "--api-key",
        "explicit-secret-must-not-be-saved",
    ])?);
    let manifest_path = workspace.path().join(".zvec-grep/manifest.json");
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    let saved = &manifest["index"]["descriptor"]["embeddingConfigs"]["http/fixture"];
    assert_eq!(saved["http"]["model"], "uncataloged-fixture-model");
    assert_eq!(saved["http"]["dimension"], 4);
    assert!(saved.get("apiKey").is_none());
    let manifest_text = std::fs::read_to_string(&manifest_path)?;
    assert!(!manifest_text.contains("must-not-be-forwarded"));
    assert!(!manifest_text.contains("explicit-secret-must-not-be-saved"));
    assert!(!workspace.path().join(".zvec-grep/credentials.key").exists());

    // Global edits cannot silently change the vector space of an existing index.
    let mut changed = config.clone();
    changed["models"]["http/fixture"]["http"]["model"] = "different-model".into();
    changed["models"]["http/fixture"]["http"]["dimension"] = 8.into();
    changed["models"]["http/fixture"]["http"]["documentPrefix"] = "different: ".into();
    changed["models"]["http/fixture"]["http"]["queryPrefix"] = "other: ".into();
    changed["models"]["http/fixture"]["http"]["metric"] = "dot_product".into();
    changed["models"]["http/fixture"]["endpoint"] = "http://127.0.0.1:1/v1".into();
    std::fs::write(&config_path, serde_json::to_vec(&changed)?)?;
    assert_command_success(&run(&[
        "--mode",
        "direct",
        "--vector",
        "authentication",
        "--refresh",
        "off",
    ])?);
    std::fs::remove_file(&config_path)?;
    assert_command_success(&run(&["--index", "--mode", "direct"])?);
    assert_command_success(&run(&["--auth", "revoke"])?);
    let previous = embedding.request_count();
    let denied = run(&["--index", "--mode", "direct"])?;
    assert!(!denied.status.success());
    let error = String::from_utf8_lossy(&denied.stderr);
    assert!(
        error.contains("authorization required") && error.contains("http/fixture"),
        "{error}"
    );
    assert_eq!(embedding.request_count(), previous);
    // The saved definition must remain sufficient to grant permission after global removal.
    assert_command_success(&run(&[
        "--auth",
        "grant",
        "--capability",
        "embedding",
        "--scope",
        "workspace",
    ])?);
    for mode in ["direct", "server", "server"] {
        let mut daemon = if mode == "server" {
            Some(start_server(&binary, &home, "full", None, configure)?.0)
        } else {
            None
        };
        let previous = embedding.request_count();
        let output = run(&[
            "--mode",
            mode,
            "--vector",
            "authentication",
            "--refresh",
            "off",
            "--color",
            "never",
        ])?;
        assert_command_success(&output);
        assert!(String::from_utf8_lossy(&output.stdout).contains("sample.md"));
        assert!(embedding.request_count() > previous);
        assert_eq!(
            embedding
                .requests
                .lock()
                .expect("requests")
                .last()
                .expect("query request")["input"],
            json!(["query: authentication"])
        );
        if let Some(guard) = daemon.as_mut() {
            let mut bridge = StdioBridge::spawn(&binary, home.path(), &guard.listen)?;
            bridge.request(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                    "clientInfo": {"name": "http-model-test", "version": "1"}}
            }))?;
            bridge.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
            let before_mcp = embedding.request_count();
            let result = bridge.request(&json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "zvec_grep_search", "arguments": {
                    "root": workspace.path(), "query": "authentication", "autoUpdate": false
                }}
            }))?;
            assert_eq!(result["result"]["isError"], false, "{result}");
            assert!(result.to_string().contains("sample.md"), "{result}");
            assert!(embedding.request_count() > before_mcp);
            bridge.close()?;
            assert_command_success(&guard.stop()?);
        }
    }
    let previous = embedding.request_count();
    let incompatible = run(&[
        "--index",
        "--mode",
        "direct",
        "--endpoint",
        "http://127.0.0.1:1/v1",
        "--allow-remote",
    ])?;
    assert!(!incompatible.status.success());
    let error = String::from_utf8_lossy(&incompatible.stderr);
    assert!(error.contains("endpoint changed; rebuild"), "{error}");
    assert_eq!(embedding.request_count(), previous);
    let requests = embedding
        .requests
        .lock()
        .expect("HTTP model fixture operation");
    assert!(
        requests
            .iter()
            .all(|r| r["model"] == "uncataloged-fixture-model")
    );
    drop(requests);
    // An explicit rebuild is the boundary for selecting a replacement definition.
    let mut replacement = config;
    replacement["models"]["http/fixture"]["http"]["model"] = "replacement-model".into();
    std::fs::write(&config_path, serde_json::to_vec(&replacement)?)?;
    assert_command_success(&run(&[
        "--index",
        "--rebuild",
        "--mode",
        "direct",
        "--allow-remote",
    ])?);
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    assert_eq!(
        manifest["index"]["descriptor"]["embeddingConfigs"]["http/fixture"]["http"]["model"],
        "replacement-model"
    );
    assert_eq!(
        embedding
            .requests
            .lock()
            .expect("requests")
            .last()
            .expect("rebuild request")["model"],
        "replacement-model"
    );
    Ok(())
}

#[test]
fn bearer_http_model_uses_env_reference_and_single_input() -> Result<(), Box<dyn Error>> {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = HttpModelServer::start(true, true)?;
    std::fs::write(
        workspace.path().join("sample.md"),
        "# Tokens\n\nTokens authorize requests.\n",
    )?;
    let config_dir = home.path().join(".zvec-grep");
    std::fs::create_dir_all(&config_dir)?;
    let config_path = config_dir.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({"version": 1, "models": {"http/fixture": {
            "endpoint": format!("http://{}/v1/embeddings", embedding.server.address),
            "http": {"model": "uncataloged-fixture-model", "dimension": 4,
                "maxInputTokens": 128, "auth": "bearer", "apiKeyEnv": "ZG_HTTP_FIXTURE_KEY",
                "singleInput": true, "queryPrefix": "query: ", "documentPrefix": "document: "}
        }}}))?,
    )?;
    let run = |args: &[&str], authenticated: bool| -> std::io::Result<Output> {
        let mut command = Command::new(&binary);
        configure_command(&mut command, home.path(), workspace.path());
        if authenticated {
            command.env("ZG_HTTP_FIXTURE_KEY", "fixture-secret");
        }
        command.args(args).output()
    };
    let args = [
        "--index",
        "--mode",
        "direct",
        "--embedding",
        "http/fixture",
        "--allow-remote",
    ];
    assert!(!run(&args, false)?.status.success());
    assert_eq!(embedding.request_count(), 0);
    assert_command_success(&run(&args, true)?);
    std::fs::remove_file(&config_path)?;
    assert_command_success(&run(
        &[
            "--mode",
            "direct",
            "--vector",
            "tokens",
            "--refresh",
            "off",
            "--allow-remote",
        ],
        true,
    )?);
    let manifest = std::fs::read_to_string(workspace.path().join(".zvec-grep/manifest.json"))?;
    assert!(manifest.contains("ZG_HTTP_FIXTURE_KEY"));
    assert!(!manifest.contains("fixture-secret"));
    assert!(!workspace.path().join(".zvec-grep/credentials.key").exists());
    assert!(embedding.request_count() >= 2);
    Ok(())
}
