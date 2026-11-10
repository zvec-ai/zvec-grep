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
    std::fs::write(
        workspace.path().join("probe.ts"),
        "function outlineProbe() {\n  const value = first();\n  second(value);\n  return value;\n}\n",
    )?;
    std::fs::write(
        workspace.path().join("checkpoint.ts"),
        "function checkpoint() {\n  return workspace();\n}\n",
    )?;
    for (name, source) in [
        (
            "python-tool",
            "#!/usr/bin/env python3\ndef python_probe():\n    return stored_call()\n",
        ),
        (
            "node-tool",
            "#!/usr/bin/env node\nfunction node_probe() { return stored_call(); }\n",
        ),
    ] {
        std::fs::write(workspace.path().join(name), source)?;
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
    let (_guard, _) = start_server(&binary, &home, "agent", None, configure)?;
    verify_positional_queries(&run, home.path())?;
    verify_previews(&run, home.path())?;
    verify_script_outlines(&run, home.path())?;
    verify_query_groups(&run, home.path())
}

fn verify_script_outlines(
    run: &impl Fn(&[&str]) -> std::io::Result<Output>,
    home: &Path,
) -> Result<(), Box<dyn Error>> {
    for (query, path) in [("python_probe", "python-tool"), ("node_probe", "node-tool")] {
        let mut outputs = Vec::new();
        for mode in ["direct", "server"] {
            let output = run(&[
                "--mode",
                mode,
                "--home",
                path_text(home)?,
                "--fts",
                query,
                "--preview",
                "detail",
                "--refresh",
                "off",
                "--color",
                "never",
            ])?;
            assert_command_success(&output);
            let text = String::from_utf8(output.stdout)?;
            assert!(text.contains(path), "{text}");
            assert!(text.contains("calls: stored_call"), "{text}");
            outputs.push(text);
        }
        assert_eq!(outputs[0], outputs[1], "{path}");
    }
    Ok(())
}

fn verify_positional_queries(
    run: &impl Fn(&[&str]) -> std::io::Result<Output>,
    home: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut direct_stdout = None;
    for mode in ["direct", "server"] {
        let search = |words: &[&str]| -> Result<Output, Box<dyn Error>> {
            let mut args = vec![
                "--mode",
                mode,
                "--home",
                path_text(home)?,
                "--allow-remote",
                "--refresh",
                "off",
                "--color",
                "never",
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
                stderr.matches("Warning:").count(),
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
            if words[0] == "--hybrid" {
                let text = String::from_utf8(output.stdout)?;
                assert_eq!(
                    text.lines().filter(|line| line.starts_with("#1 ")).count(),
                    2,
                    "{text}"
                );
                assert_eq!(text.matches("source.txt").count(), 2, "{text}");
            } else {
                let limited_output = if words.contains(&"--limit=3") {
                    Some(search(&["connection pool timeout", "--limit=3"])?)
                } else {
                    None
                };
                let expected = limited_output.as_ref().unwrap_or(&expected);
                assert_eq!(
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&expected.stdout),
                    "{mode}: {words:?}"
                );
            }
        }
    }
    Ok(())
}

fn verify_previews(
    run: &impl Fn(&[&str]) -> std::io::Result<Output>,
    home: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut default_output = None;
    for preview in [
        None,
        Some("none"),
        Some("short"),
        Some("full"),
        Some("detail"),
    ] {
        let mut outputs = Vec::new();
        for mode in ["direct", "server"] {
            let mut args = vec![
                "--mode",
                mode,
                "--home",
                path_text(home)?,
                "--fts",
                "outlineProbe",
                "--refresh",
                "off",
                "--color",
                "never",
            ];
            if let Some(preview) = preview {
                args.extend(["--preview", preview]);
            }
            let output = run(&args)?;
            assert_command_success(&output);
            let text = String::from_utf8(output.stdout)?;
            assert!(text.contains("probe.ts"), "{text}");
            if preview == Some("detail") {
                assert!(text.contains("calls: first, second"), "{text}");
                for field in [
                    "Range:",
                    "Status: fresh",
                    "Kind: code/function",
                    "Symbol: outlineProbe",
                    "Signature: function outlineProbe()",
                    "Outline:",
                    "Source:",
                ] {
                    assert!(text.contains(field), "missing {field}: {text}");
                }
            }
            if matches!(preview, None | Some("short" | "full")) {
                assert!(text.contains("\n  1: function outlineProbe() {"), "{text}");
                assert!(!text.contains("outline:"), "{text}");
                assert!(!text.contains("source:"), "{text}");
            }
            for metadata in [
                "matchedBy=",
                "hits:",
                "Hits:",
                "query groups",
                "Groups:",
                "Group:",
                "Query:",
                "Q1 [",
            ] {
                assert!(!text.contains(metadata), "{preview:?}: {text}");
            }
            outputs.push(text);
        }
        assert_eq!(outputs[0], outputs[1], "preview={preview:?}");
        if preview.is_none() {
            default_output = Some(outputs[0].clone());
        }
        if preview == Some("short") {
            assert_eq!(default_output.as_ref(), Some(&outputs[0]));
        }
    }
    Ok(())
}

fn verify_query_groups(
    run: &impl Fn(&[&str]) -> std::io::Result<Output>,
    home: &Path,
) -> Result<(), Box<dyn Error>> {
    for preview in ["none", "short", "full", "detail"] {
        for fuse in [false, true] {
            let mut outputs = Vec::new();
            for mode in ["direct", "server"] {
                let mut args = vec![
                    "--mode",
                    mode,
                    "--home",
                    path_text(home)?,
                    "--hybrid",
                    "checkpoint",
                    "--fts",
                    "workspace",
                    "--preview",
                    preview,
                    "--allow-remote",
                    "--refresh",
                    "off",
                    "--color",
                    "never",
                ];
                if fuse {
                    args.push("--fuse");
                }
                let output = run(&args)?;
                assert_command_success(&output);
                let text = String::from_utf8(output.stdout)?;
                let count = if fuse { 1 } else { 2 };
                assert_eq!(
                    text.lines()
                        .filter(|line| line.trim_start().starts_with("#1 "))
                        .count(),
                    count,
                    "{mode} {preview} fuse={fuse}: {text}"
                );
                for metadata in [
                    "matchedBy=",
                    "hits:",
                    "Hits:",
                    "query groups",
                    "Groups:",
                    "Group:",
                    "Query:",
                    "Q1 [",
                    "Q2 [",
                ] {
                    assert!(!text.contains(metadata), "{text}");
                }
                assert_eq!(
                    text.matches("checkpoint.ts").count(),
                    count,
                    "{mode} {preview} fuse={fuse}: {text}"
                );
                outputs.push(text);
            }
            assert_eq!(outputs[0], outputs[1], "{preview} fuse={fuse}");
        }
    }
    Ok(())
}
