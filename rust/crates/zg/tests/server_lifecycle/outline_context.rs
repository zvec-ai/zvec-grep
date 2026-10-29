use super::*;

const FIXTURES: [(&str, &str, &str, Option<&str>); 7] = [
    (
        "component.vue",
        "<script lang=\"tsx\">\nfunction render() { return <Widget title={formatTitle()}>{renderBody()}</Widget>; }\n</script>\n",
        "render",
        Some("calls: formatTitle, renderBody"),
    ),
    (
        "component.svelte",
        "<script lang=\"tsx\">\nclass View { render() { return <Widget title={formatTitle()}>{renderBody()}</Widget>; } }\n</script>\n",
        "render",
        Some("calls: formatTitle, renderBody"),
    ),
    (
        "Builder.java",
        "class Builder { Builder() { init(); } }\n",
        "Builder",
        Some("calls: init"),
    ),
    (
        "store.js",
        "class Store { get(key) { function get() { nested(); } return load(key); } }\n",
        "get",
        Some("calls: nested, load"),
    ),
    (
        "reader.go",
        "package p\ntype Reader interface {\nRead()\nClose() error\n}\n",
        "Read",
        None,
    ),
    (
        "reader.go",
        "package p\ntype Reader interface {\nRead()\nClose() error\n}\n",
        "Close",
        None,
    ),
    (
        "consume.go",
        "package p\nfunc Consume() { Read(); Consume() }\n",
        "Consume",
        Some("calls: Read, Consume"),
    ),
];

#[test]
fn stored_outline_context_is_consistent_in_direct_and_server_modes() -> Result<(), Box<dyn Error>> {
    let _permit = server_test_permit();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_zg"));
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let embedding = EmbeddingServer::start()?;
    for (path, source, _, _) in FIXTURES {
        std::fs::write(workspace.path().join(path), source)?;
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
    for (path, _, query, expected_calls) in FIXTURES {
        let mut outputs = Vec::new();
        for mode in ["direct", "server"] {
            let output = run(&[
                "--mode",
                mode,
                "--home",
                path_text(home.path())?,
                "--fts",
                query,
                "--glob",
                path,
                "--symbol-type",
                "function",
                "--preview",
                "detail",
                "--refresh",
                "off",
                "--no-color",
            ])?;
            assert_command_success(&output);
            let text = String::from_utf8(output.stdout)?;
            assert!(text.contains(path), "{text}");
            assert!(text.contains(&format!("Symbol: {query}")), "{text}");
            let calls = text
                .lines()
                .map(str::trim)
                .filter(|line| line.starts_with("calls:"))
                .collect::<Vec<_>>();
            assert_eq!(
                calls,
                expected_calls.into_iter().collect::<Vec<_>>(),
                "{path}, {mode}: {text}"
            );
            outputs.push(text);
        }
        assert_eq!(outputs[0], outputs[1], "{path}");
    }
    Ok(())
}
