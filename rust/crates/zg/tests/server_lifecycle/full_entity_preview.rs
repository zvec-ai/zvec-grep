use super::*;

use zg_daemon_protocol::{DaemonCommand, DaemonReply, ExecutionResult};
use zg_engine::api::context::{
    ContextOptions, ContextResult,
    options::{ContextPreviewMode, ContextRoute, ContextRouteMode, RefreshPolicy},
    result::{ContentRange, ContextItem},
};

fn fixtures() -> Vec<(&'static str, String, String, String)> {
    let paragraph = "The indexed entity keeps its original context even when retrieval splits it into overlapping fragments.\n\n";
    let statement = "    observe('The indexed function keeps its context across overlapping retrieval fragments.');\n";
    vec![
        (
            "long.md",
            "# Neighbor before\n\nexcludedbefore\n\n".into(),
            format!(
                "# Complete entity\n\nentitybegin\n\n{}quasarneedle\n\n{}entityend\n\n",
                paragraph.repeat(40),
                paragraph.repeat(40),
            ),
            "# Neighbor after\n\nexcludedafter\n".into(),
        ),
        (
            "long.ts",
            "function before() { return 'excludedbefore'; }\n\n".into(),
            format!(
                "function entitybegin() {{\n{}    const needle = 'quasarneedle';\n{}    return 'entityend';\n}}",
                statement.repeat(40),
                statement.repeat(40),
            ),
            "\n\nfunction after() { return 'excludedafter'; }\n".into(),
        ),
    ]
}

#[test]
fn full_preview_returns_complete_markdown_and_code_entities_in_direct_and_server_modes()
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
    verify_previews(&run, home.path())?;
    verify_server_previews(&guard.listen, workspace.path())
}

fn verify_server_previews(listen: &str, root: &Path) -> Result<(), Box<dyn Error>> {
    let request = ContextOptions {
        root: Some(root.to_owned()),
        routes: vec![ContextRoute {
            mode: ContextRouteMode::Fts,
            query: "quasarneedle".to_owned(),
        }],
        limit: Some(2),
        auto_update: false,
        refresh: Some(RefreshPolicy::Off),
        ..ContextOptions::default()
    };
    let query = |preview| {
        read_preview_response(
            listen,
            &DaemonCommand::ContextPreview {
                request: request.clone(),
                preview,
            },
        )
    };
    let (full_body, full) = query(ContextPreviewMode::Full)?;
    assert_eq!(full.items.len(), 2);
    assert_eq!(full.group_results.len(), 1);
    assert_eq!(full.group_results[0].items.len(), 2);
    for item in response_items(&full) {
        let (_, prefix, entity, _) = fixtures()
            .into_iter()
            .find(|(path, ..)| item.relative_path == Path::new(path))
            .expect("indexed fixture");
        assert_eq!(item.content.trim_end(), entity.trim_end());
        assert_eq!(
            item.content_range.start_line(),
            Some(prefix.lines().count() + 1)
        );
        assert!(item.source_preview.is_none());
    }
    for preview in [ContextPreviewMode::None, ContextPreviewMode::Short] {
        let (body, result) = query(preview)?;
        assert!(
            body.len() < full_body.len(),
            "{preview:?} response is bounded"
        );
        for excluded in [
            "entityend",
            "excludedbefore",
            "excludedafter",
            "changedondisk",
        ] {
            assert!(!body.contains(excluded), "{preview:?}: {body}");
        }
        assert_eq!(result.items.len(), full.items.len());
        assert_eq!(result.group_results.len(), full.group_results.len());
        assert_eq!(
            result.group_results[0].items.len(),
            full.group_results[0].items.len()
        );
        for (item, original) in response_items(&result).zip(response_items(&full)) {
            assert_same_retrieval(item, original);
            assert!(
                item.content.is_empty(),
                "raw entity body must not cross HTTP"
            );
            let ContentRange::Text {
                start_byte_offset,
                end_byte_offset,
                ..
            } = &item.content_range
            else {
                panic!("source preview retains an exact empty content range");
            };
            assert_eq!(start_byte_offset, end_byte_offset);
            let source = item
                .source_preview
                .as_ref()
                .expect("prepared source preview");
            let max_lines = if preview == ContextPreviewMode::None {
                1
            } else {
                10
            };
            assert!(!source.lines.is_empty() && source.lines.len() <= max_lines);
            for line in &source.lines {
                assert!(line.line_number.is_some());
                assert!(line.text.chars().count() <= 198, "bounded source line");
            }
        }
    }
    let (_, detail) = query(ContextPreviewMode::Detail)?;
    assert_eq!(
        response_items(&detail).count(),
        response_items(&full).count()
    );
    for (item, original) in response_items(&detail).zip(response_items(&full)) {
        assert_same_retrieval(item, original);
        assert_eq!(item.content, original.content);
        assert_eq!(item.content_range, original.content_range);
        assert!(item.source_preview.is_none());
    }
    Ok(())
}

fn response_items(result: &ContextResult) -> impl Iterator<Item = &ContextItem> {
    result
        .items
        .iter()
        .chain(result.group_results.iter().flat_map(|group| &group.items))
}

fn assert_same_retrieval(item: &ContextItem, original: &ContextItem) {
    let identity = |item| {
        let mut value = serde_json::to_value(item).expect("context item");
        let fields = value.as_object_mut().expect("context item object");
        for field in ["content", "content_range", "source_preview", "outline"] {
            fields.remove(field);
        }
        value
    };
    assert_eq!(
        identity(item),
        identity(original),
        "preview preserves retrieval metadata"
    );
}

fn read_preview_response(
    listen: &str,
    command: &DaemonCommand,
) -> Result<(String, ContextResult), Box<dyn Error>> {
    let body = serde_json::to_string(command)?;
    let mut stream = TcpStream::connect(listen)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "POST /admin/execute HTTP/1.1\r\nHost: {listen}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    let response = read_http_response(&mut stream)?;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let (_, body) = response
        .split_once("\r\n\r\n")
        .ok_or("HTTP response body")?;
    let ExecutionResult::Success(DaemonReply::Context(result)) = serde_json::from_str(body)? else {
        return Err(format!("unexpected preview response: {body}").into());
    };
    Ok((body.to_owned(), *result))
}

fn verify_previews(
    run: &impl Fn(&[&str]) -> std::io::Result<Output>,
    home: &Path,
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
        }
    }
    Ok(())
}
