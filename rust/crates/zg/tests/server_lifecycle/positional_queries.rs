use super::*;

#[test]
fn positional_queries_merge_and_warn_consistently_in_direct_and_server_modes()
-> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    std::fs::write(
        workspace.path().join("source.txt"),
        "connection pool timeout\nsocket timeout\n",
    )?;
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
    let (_guard, _) = start_server(&binary, &home, "agent", None, configure)?;
    let mut direct_stdout = None;
    for mode in ["direct", "server"] {
        let search = |words: &[&str]| -> Result<Output, Box<dyn Error>> {
            let mut args = vec![
                "--mode",
                mode,
                "--home",
                path_text(home.path())?,
                "--allow-remote",
                "--refresh",
                "off",
                "--compact",
                "--no-color",
            ];
            args.extend(words);
            let output = run(&args)?;
            assert_command_success(&output);
            Ok(output)
        };
        let expected = search(&["connection pool timeout"])?;
        assert!(String::from_utf8_lossy(&expected.stdout).contains("source.txt"));
        if mode == "direct" {
            direct_stdout = Some(expected.stdout.clone());
        } else {
            assert_eq!(Some(&expected.stdout), direct_stdout.as_ref());
        }
        for (words, warns) in [
            (vec!["connection", "pool", "timeout"], false),
            (vec!["connection pool", "timeout"], true),
            (vec!["connection pool", "socket timeout"], true),
            (vec!["connection", "pool", "--limit=3", "timeout"], false),
            (vec!["--", "connection pool", "timeout"], true),
            (
                vec!["--hybrid", "connection pool", "--hybrid", "socket timeout"],
                false,
            ),
        ] {
            let output = search(&words)?;
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                stderr.matches("warning:").count(),
                usize::from(warns),
                "{mode}: {words:?}: {stderr}"
            );
            if warns {
                assert!(
                    stderr.contains("positional query arguments are merged into one query:"),
                    "{stderr}"
                );
                assert!(
                    stderr.contains("zg --hybrid \"connection pool\" --hybrid \"socket timeout\""),
                    "{stderr}"
                );
            }
            assert_eq!(output.stdout, expected.stdout, "{mode}: {words:?}");
        }
    }
    Ok(())
}
