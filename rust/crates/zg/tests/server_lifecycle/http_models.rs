use super::*;

struct HttpModelServer {
    server: EmbeddingServer,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    authorizations: Arc<Mutex<Vec<Option<String>>>>,
}

impl HttpModelServer {
    fn start(single_input: bool) -> std::io::Result<Self> {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let authorizations = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let captured_auth = Arc::clone(&authorizations);
        let server = EmbeddingServer::start_with_handler(move |mut stream, _| {
            let (headers, request) = read_embedding_request(&stream)?;
            assert!(headers.starts_with("POST /v1/embeddings HTTP/1.1\r\n"));
            let authorization = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.trim().to_owned());
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
            captured_auth
                .lock()
                .expect("authorizations")
                .push(authorization);
            write_embedding_response(&mut stream, &json!({"data": data}))
        })?;
        Ok(Self {
            server,
            requests,
            authorizations,
        })
    }

    fn request_count(&self) -> usize {
        self.server.requests.load(Ordering::SeqCst)
    }

    fn last_authorization(&self) -> Option<String> {
        self.authorizations
            .lock()
            .expect("authorizations")
            .last()
            .cloned()
            .flatten()
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

fn grant_model(model: &str) -> [&str; 8] {
    [
        "--auth",
        "grant",
        "--capability",
        "embedding",
        "--scope",
        "workspace",
        "--embedding",
        model,
    ]
}

fn write_routed_model_config(
    home: &Path,
    docs: &HttpModelServer,
    code: &HttpModelServer,
) -> Result<PathBuf, Box<dyn Error>> {
    let config_dir = home.join(".zvec-grep");
    std::fs::create_dir_all(&config_dir)?;
    let config_path = config_dir.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({"version": 1, "models": {
            "http/docs": {
                "endpoint": format!("http://{}/v1", docs.server.address),
                "model": "docs-encoder", "dimension": 4, "maxInputTokens": 128,
                "queryPrefix": "query: ", "documentPrefix": "document: ", "auth": "none"
            },
            "http/fixture": {
                "endpoint": format!("http://{}/v1", code.server.address),
                "model": "code-encoder", "dimension": 4, "maxInputTokens": 128,
                "queryPrefix": "query: ", "documentPrefix": "document: ",
                "auth": "bearer", "apiKeyEnv": "ZG_HTTP_FIXTURE_KEY"
            }
        }}))?,
    )?;
    Ok(config_path)
}

#[test]
fn code_route_reuses_its_saved_http_model_and_consent_with_a_local_default()
-> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let docs = HttpModelServer::start(false)?;
    let code = HttpModelServer::start(false)?;
    let root = workspace.path();
    std::fs::write(root.join("README.md"), "Use the authentication guide.\n")?;
    let config_path = write_routed_model_config(home.path(), &docs, &code)?;
    let configure = |command: &mut Command| configure_command(command, home.path(), root);
    let run = |args: &[&str]| {
        let mut command = Command::new(&binary);
        configure(&mut command);
        command.args(args).output()
    };
    let query = |mode, route, refresh| {
        run(&[
            "--mode",
            mode,
            route,
            "authenticate",
            "--kind",
            "code",
            "--refresh",
            refresh,
        ])
    };
    // Both endpoints are local fixtures. Only the text route has consent.
    assert_command_success(&run(&grant_model("http/docs"))?);
    assert_command_success(&run(&[
        "--mode",
        "direct",
        "--index",
        "--embedding",
        "local/potion-code-16m-v2",
        "--embedding",
        "text=http/docs",
        "--embedding",
        "code=http/fixture",
    ])?);
    assert!(docs.request_count() > 0);
    assert!(root.join(".zvec-grep/index/text").is_dir());
    let code_dir = root.join(".zvec-grep/index/code");
    std::fs::remove_file(config_path)?;
    // The index requires its configured code collection even before the first code file.
    let hidden_code = code_dir.with_file_name("missing-code");
    std::fs::rename(&code_dir, &hidden_code)?;
    let damaged = query("direct", "--fts", "off")?;
    assert!(!damaged.status.success());
    assert!(
        String::from_utf8_lossy(&damaged.stderr)
            .contains("index collection code is missing; rebuild the index"),
        "{damaged:?}"
    );
    std::fs::rename(hidden_code, &code_dir)?;

    let (mut daemon, _) = start_server(&binary, &home, "full", None, configure)?;
    for mode in ["direct", "server"] {
        for route in ["--vector", "--fts"] {
            let result = query(mode, route, "off")?;
            assert_command_success(&result);
            assert_eq!(String::from_utf8(result.stdout)?, "No matches.\n");
        }
    }
    assert_eq!(code.request_count(), 0);

    std::fs::write(root.join("lib.rs"), "pub fn authenticate() {}\n")?;
    verify_legacy_background_consent(home.path(), root)?;
    assert_command_success(&daemon.stop()?);
    let denied = query("direct", "--vector", "wait")?;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("http/fixture"));
    assert_eq!(code.request_count(), 0);
    assert_command_success(&run(&grant_model("http/fixture"))?);
    let (mut daemon, _) = start_server(&binary, &home, "full", None, |command| {
        configure(command);
        command.env("ZG_HTTP_FIXTURE_KEY", "code-route-key");
    })?;
    let result = query("server", "--vector", "wait")?;
    assert_command_success(&result);
    assert!(String::from_utf8_lossy(&result.stdout).contains("lib.rs"));
    assert!(code.request_count() >= 2, "document and query embeddings");
    assert_eq!(
        code.last_authorization().as_deref(),
        Some("Bearer code-route-key")
    );
    let previous = code.request_count();
    assert_command_success(&daemon.stop()?);
    assert_command_success(&run(&["--auth", "revoke"])?);
    std::fs::remove_file(root.join("lib.rs"))?;
    let result = query("direct", "--vector", "wait")?;
    assert_command_success(&result);
    assert_eq!(String::from_utf8(result.stdout)?, "No matches.\n");
    assert_eq!(code.request_count(), previous);
    assert!(code_dir.is_dir());
    Ok(())
}

fn verify_legacy_background_consent(home: &Path, root: &Path) -> Result<(), Box<dyn Error>> {
    use zg_daemon_protocol::{DaemonCommand, DaemonReply};
    use zg_engine::api::{
        context::{
            ContextOptions,
            options::{ContextRoute, ContextRouteMode},
        },
        index::options::ContentKind,
    };
    let preflight = DaemonCommand::QueryAuthorization(ContextOptions {
        root: Some(root.to_owned()),
        target_kinds: vec![ContentKind::Code],
        routes: vec![ContextRoute {
            mode: ContextRouteMode::Vector,
            query: "authenticate".into(),
        }],
        auto_update: true,
        refresh: None,
        ..ContextOptions::default()
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let DaemonReply::QueryAuthorization(grants) =
        runtime.block_on(zg_daemon::execute_command(home, preflight))?
    else {
        panic!("expected query preflight response");
    };
    // A new code file needs a document embedding. The current index has no code to query.
    assert_eq!(grants.len(), 1);
    assert!(grants[0].workspace_content);
    assert!(!grants[0].query_text);
    Ok(())
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
    let embedding = HttpModelServer::start(false)?;
    std::fs::write(
        workspace.path().join("sample.md"),
        "# Authentication\n\nA token authenticates each request.\n",
    )?;
    let config_dir = home.path().join(".zvec-grep");
    std::fs::create_dir_all(&config_dir)?;
    let config_path = config_dir.join("config.json");
    let config = json!({"version": 1, "models": {"http/fixture": {
        "endpoint": format!("http://{}/v1", embedding.server.address),
        "model": "uncataloged-fixture-model",
        "dimension": 4,
        "maxInputTokens": 128,
        "maxBatchSize": 2,
        "auth": "none",
        "queryPrefix": "query: ",
        "documentPrefix": "document: "
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
    assert_command_success(&run(&grant_model("http/fixture"))?);
    assert_command_success(&run(&[
        "--index",
        "--mode",
        "direct",
        "--api-key",
        "explicit-secret-must-not-be-saved",
    ])?);
    assert_eq!(embedding.last_authorization(), None);
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
    changed["models"]["http/fixture"]["model"] = "different-model".into();
    changed["models"]["http/fixture"]["dimension"] = 8.into();
    changed["models"]["http/fixture"]["documentPrefix"] = "different: ".into();
    changed["models"]["http/fixture"]["queryPrefix"] = "other: ".into();
    changed["models"]["http/fixture"]["metric"] = "dot_product".into();
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
    assert_command_success(&run(&["--index", "--mode", "direct"])?);
    assert_eq!(embedding.request_count(), previous);
    std::fs::write(
        workspace.path().join("sample.md"),
        "# Authentication\n\nEach request needs an updated authentication token.\n",
    )?;
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
    assert_command_success(&run(&["--index", "--mode", "direct"])?);
    assert!(embedding.request_count() > previous);
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
        assert_eq!(embedding.last_authorization(), None);
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
    replacement["models"]["http/fixture"]["model"] = "replacement-model".into();
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
#[expect(
    clippy::too_many_lines,
    reason = "Verify env fallback, encrypted indexing save, temporary query override, and CLI-to-daemon forwarding"
)]
fn bearer_http_model_honors_api_key_precedence_persistence_and_daemon_forwarding()
-> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = HttpModelServer::start(true)?;
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
            "model": "uncataloged-fixture-model",
            "dimension": 4,
            "maxInputTokens": 128,
            "auth": "bearer",
            "apiKeyEnv": "ZG_HTTP_FIXTURE_KEY",
            "singleInput": true,
            "queryPrefix": "query: ",
            "documentPrefix": "document: "
        }}}))?,
    )?;
    let run = |args: &[&str], env_key: Option<&str>| -> std::io::Result<Output> {
        let mut command = Command::new(&binary);
        configure_command(&mut command, home.path(), workspace.path());
        if let Some(key) = env_key {
            command.env("ZG_HTTP_FIXTURE_KEY", key);
        }
        command.args(args).output()
    };
    let index_args = [
        "--index",
        "--mode",
        "direct",
        "--embedding",
        "http/fixture",
        "--allow-remote",
    ];
    // Missing key fails without falling back to ZVEC_GREP_API_KEY.
    assert!(!run(&index_args, None)?.status.success());
    assert_eq!(embedding.request_count(), 0);

    // apiKeyEnv works as fallback and leaves no credentials.key on disk.
    assert_command_success(&run(&index_args, Some("env-secret"))?);
    assert_eq!(
        embedding.last_authorization().as_deref(),
        Some("Bearer env-secret")
    );
    let manifest_path = workspace.path().join(".zvec-grep/manifest.json");
    let manifest_text = std::fs::read_to_string(&manifest_path)?;
    assert!(manifest_text.contains("ZG_HTTP_FIXTURE_KEY"));
    assert!(!manifest_text.contains("env-secret"));
    assert!(!workspace.path().join(".zvec-grep/credentials.key").exists());

    // Query --api-key overrides apiKeyEnv temporarily and does not persist.
    assert_command_success(&run(
        &[
            "--mode",
            "direct",
            "--vector",
            "tokens",
            "--refresh",
            "off",
            "--allow-remote",
            "--api-key",
            "query-temp-secret",
        ],
        Some("env-secret"),
    )?);
    assert_eq!(
        embedding.last_authorization().as_deref(),
        Some("Bearer query-temp-secret")
    );
    assert!(!workspace.path().join(".zvec-grep/credentials.key").exists());

    // Indexing via daemon with --api-key overrides the daemon's apiKeyEnv and saves encrypted.
    let (mut daemon, _) = start_server(&binary, &home, "full", None, |command| {
        configure_command(command, home.path(), workspace.path());
        command.env("ZG_HTTP_FIXTURE_KEY", "env-secret");
    })?;
    std::fs::write(
        workspace.path().join("sample.md"),
        "# Tokens\n\nUpdated token rules authorize requests.\n",
    )?;
    assert_command_success(&run(
        &[
            "--index",
            "--mode",
            "server",
            "--allow-remote",
            "--api-key",
            "indexed-saved-secret",
        ],
        None,
    )?);
    assert_eq!(
        embedding.last_authorization().as_deref(),
        Some("Bearer indexed-saved-secret")
    );
    assert_command_success(&daemon.stop()?);
    let manifest_text = std::fs::read_to_string(&manifest_path)?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text)?;
    assert!(
        manifest["index"]["descriptor"]["embeddingConfigs"]["http/fixture"]["apiKey"].is_object()
    );
    assert!(!manifest_text.contains("indexed-saved-secret"));
    assert!(workspace.path().join(".zvec-grep/credentials.key").exists());

    // After removing global config and restarting the daemon without apiKeyEnv,
    // saved encrypted credentials serve queries, query --api-key overrides them
    // temporarily across the daemon boundary, and saved credentials remain intact.
    std::fs::remove_file(&config_path)?;
    let (mut daemon, _) = start_server(&binary, &home, "full", None, |command| {
        configure_command(command, home.path(), workspace.path());
    })?;
    assert_command_success(&run(
        &[
            "--mode",
            "server",
            "--vector",
            "tokens",
            "--refresh",
            "off",
            "--allow-remote",
        ],
        None,
    )?);
    assert_eq!(
        embedding.last_authorization().as_deref(),
        Some("Bearer indexed-saved-secret")
    );
    assert_command_success(&run(
        &[
            "--mode",
            "server",
            "--vector",
            "tokens",
            "--refresh",
            "off",
            "--allow-remote",
            "--api-key",
            "server-query-override",
        ],
        None,
    )?);
    assert_eq!(
        embedding.last_authorization().as_deref(),
        Some("Bearer server-query-override")
    );
    assert_command_success(&run(
        &[
            "--mode",
            "server",
            "--vector",
            "tokens",
            "--refresh",
            "off",
            "--allow-remote",
        ],
        None,
    )?);
    assert_eq!(
        embedding.last_authorization().as_deref(),
        Some("Bearer indexed-saved-secret")
    );
    assert_command_success(&daemon.stop()?);

    // Consent preflight rejects unauthorized runs without reading credentials or contacting the server.
    let previous = embedding.request_count();
    let denied = run(
        &["--mode", "direct", "--vector", "tokens", "--refresh", "off"],
        None,
    )?;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("authorization required"));
    assert_eq!(embedding.request_count(), previous);
    Ok(())
}
