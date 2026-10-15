use super::*;

#[test]
fn code_keywords_match_explicit_fts_without_embedding_in_direct_and_server()
-> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    std::fs::write(
        workspace.path().join("sample.rs"),
        "pub fn verifyToken() {}\npub fn request_service() {}\npub struct Request;\nimpl Request { pub fn Service() {} }\nfn caller() { Request::Service(); }\n",
    )?;
    let registry = home.path().join("workspaces.json");
    let index = Command::new(&binary)
        .current_dir(workspace.path())
        .env("ZVEC_GREP_HOME", home.path())
        .env("ZVEC_GREP_WORKSPACE_REGISTRY", &registry)
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
    assert_command_success(&index);
    let requests = embedding.requests.load(Ordering::SeqCst);
    assert!(requests > 0);
    embedding.fail.store(true, Ordering::SeqCst);
    let (mut guard, _) = start_server(&binary, &home, "full", None, |command| {
        command.env("ZVEC_GREP_WORKSPACE_REGISTRY", &registry);
    })?;
    for query in ["verifyToken", "request_service", "Request::Service"] {
        let mut previous = None;
        for mode in ["direct", "server"] {
            for explicit in [false, true] {
                let mut command = Command::new(&binary);
                command
                    .current_dir(workspace.path())
                    .env("ZVEC_GREP_HOME", home.path())
                    .env("ZVEC_GREP_WORKSPACE_REGISTRY", &registry)
                    .args(["--mode", mode, "--refresh", "off", "--debug"]);
                if explicit {
                    command.arg("--fts");
                }
                let reply = command.arg(query).output()?;
                assert_command_success(&reply);
                let stdout = String::from_utf8(reply.stdout)?;
                assert!(stdout.contains("matchedBy=fts sample.rs:"), "{stdout}");
                if let Some(previous) = &previous {
                    assert_eq!(&stdout, previous, "{query} {mode} explicit={explicit}");
                }
                previous = Some(stdout);
                let stderr = String::from_utf8(reply.stderr)?;
                let diagnostics: serde_json::Value = serde_json::from_str(
                    stderr
                        .lines()
                        .find_map(|line| line.strip_prefix("Diagnostics: "))
                        .ok_or("missing diagnostics")?,
                )?;
                let routes = diagnostics["index"]["routes"]
                    .as_array()
                    .ok_or("missing routes")?;
                assert_eq!(routes.len(), 1);
                assert_eq!(routes[0]["mode"], "fts");
                assert!(diagnostics["rg"].is_null());
            }
        }
    }
    assert_eq!(embedding.requests.load(Ordering::SeqCst), requests);
    assert_command_success(&guard.stop()?);
    Ok(())
}
