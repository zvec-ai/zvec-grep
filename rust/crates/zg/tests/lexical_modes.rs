use std::{error::Error, fs, process::Command};

use tempfile::TempDir;

#[test]
fn managed_rg_preserves_patterns_and_case_insensitive_globs() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    fs::write(root.path().join("sample.rs"), "needle\n needle \nother\n")?;
    fs::write(root.path().join("sample.txt"), "needle\n")?;
    let cases: &[(&[&str], &str)] = &[
        (
            &["-F", " needle ", "sample.rs"],
            "sample.rs\n  2:  needle\n",
        ),
        (&["-F", " ", "sample.rs"], "sample.rs\n  2:  needle\n"),
        (
            &["--iglob", "*.RS", "needle", "."],
            "sample.rs\n  1: needle\n  2:  needle\n",
        ),
        (
            &["-e", "", "sample.rs"],
            "sample.rs\n  1: needle\n  2:  needle\n  3: other\n",
        ),
    ];
    for mode in ["direct", "server"] {
        for (args, expected) in cases {
            let output = Command::new(env!("CARGO_BIN_EXE_zg"))
                .current_dir(root.path())
                .args(["query", "--mode", mode, "--rg"])
                .args(*args)
                .output()?;
            assert!(
                output.status.success(),
                "{mode} {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8(output.stdout)?,
                *expected,
                "{mode} {args:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn direct_and_server_modes_share_embedded_lexical_behavior() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    fs::write(
        root.path().join("sample.txt"),
        "before\nresident keyword search\nafter\n",
    )?;
    let binary = env!("CARGO_BIN_EXE_zg");
    let run = |mode: &str| {
        Command::new(binary)
            .current_dir(root.path())
            .args([
                "query",
                "--mode",
                mode,
                "--rg",
                "-F",
                "resident keyword",
                ".",
            ])
            .output()
    };

    let direct = run("direct")?;
    let server = run("server")?;
    assert!(
        direct.status.success(),
        "direct stderr: {}",
        String::from_utf8_lossy(&direct.stderr)
    );
    assert!(
        server.status.success(),
        "server stderr: {}",
        String::from_utf8_lossy(&server.stderr)
    );
    assert_eq!(direct.stdout, server.stdout);
    assert_eq!(
        String::from_utf8(direct.stdout)?,
        "sample.txt\n  2: resident keyword search\n"
    );
    Ok(())
}

#[test]
fn managed_rg_preserves_mixed_glob_order_before_and_across_rg() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    fs::write(root.path().join("sample.rs"), "needle\n")?;
    fs::write(root.path().join("unrelated.txt"), "needle\n")?;
    let cases: &[(&[&str], bool)] = &[
        (&["--iglob", "*.RS", "--glob", "!*.rs", "--rg"], false),
        (&["--glob", "!*.rs", "--iglob", "*.RS", "--rg"], true),
        (&["--iglob", "*.RS", "--rg", "--glob", "!*.rs"], false),
        (&["--glob", "!*.rs", "--rg", "--iglob", "*.RS"], true),
    ];
    for (args, included) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_zg"))
            .current_dir(root.path())
            .arg("query")
            .args(*args)
            .args(["needle", "."])
            .output()?;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout)?,
            if *included {
                "sample.rs\n  1: needle\n"
            } else {
                "No matches.\n"
            },
            "{args:?}"
        );
    }
    Ok(())
}

async fn search_args(
    root: &std::path::Path,
    args: &[&str],
) -> Result<zg_engine::api::context::ContextResult, Box<dyn Error>> {
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    let mut request = zg_cli::parse_managed_rg_args(&args)?;
    request.root = Some(root.to_path_buf());
    // Exercise the same serialized options used across the daemon boundary.
    let request = serde_json::from_value(serde_json::to_value(request)?)?;
    Ok(zg_engine::ZvecGrep::new().context(request).await?)
}

fn start_lines(reply: &zg_engine::api::context::ContextResult) -> Vec<usize> {
    reply
        .items
        .iter()
        .map(|item| match item.range {
            zg_engine::api::context::result::ContentRange::Text { start_line, .. } => start_line,
            _ => panic!("expected text coordinates"),
        })
        .collect()
}

#[tokio::test]
async fn managed_rg_matching_options_preserve_source_coordinates() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    fs::write(
        root.path().join("sample.txt"),
        "Needle\nneedle\nneedle suffix\nother\nfirst\nsecond\nlast\n",
    )?;
    let cases: &[(&[&str], &[usize])] = &[
        (&["-S", "needle"], &[1, 2, 3]),
        (&["-S", "Needle"], &[1]),
        (&["-is", "needle"], &[2, 3]),
        (&["-si", "needle"], &[1, 2, 3]),
        (&["-i", "--case-sensitive", "needle"], &[2, 3]),
        (&["-ix", "needle"], &[1, 2]),
        (&["-iv", "needle"], &[4, 5, 6, 7]),
        (&["-U", "first\\nsecond"], &[5]),
        (&["-U", "--multiline-dotall", "first.*second"], &[5]),
        (&["-U", "first.*second"], &[]),
        (&["-U", "e"], &[1, 2, 3, 4, 6]),
        (&["-F", "--no-fixed-strings", "^needle$"], &[2]),
        (&["--regexp="], &[1, 2, 3, 4, 5, 6, 7]),
        (&["--max-count", "1", "-i", "needle"], &[1]),
        (&["-m0", "needle"], &[]),
        (&["-i", "--stop-on-nonmatch", "needle"], &[1, 2, 3]),
        (
            &[
                "--engine=default",
                "--regex-size-limit=10M",
                "--dfa-size-limit",
                "10M",
                "needle",
            ],
            &[2, 3],
        ),
    ];
    for (args, expected) in cases {
        let mut args = args.to_vec();
        args.push("sample.txt");
        let reply = search_args(root.path(), &args).await?;
        assert_eq!(start_lines(&reply), *expected, "{args:?}");
    }
    let reply = search_args(root.path(), &["-U", "first\\nsecond", "sample.txt"]).await?;
    assert_eq!(reply.items[0].content, "first\nsecond");
    assert!(matches!(
        reply.items[0].range,
        zg_engine::api::context::result::ContentRange::Text {
            start_line: 5,
            end_line: 6,
            ..
        }
    ));
    let reply = search_args(root.path(), &["-im1", "needle", "sample.txt"]).await?;
    assert_eq!(
        reply.coverage,
        zg_engine::api::context::result::ContextCoverage::RgTruncated
    );
    let reply = search_args(root.path(), &["-m1", "other", "sample.txt"]).await?;
    assert_eq!(
        reply.coverage,
        zg_engine::api::context::result::ContextCoverage::RgExhaustive
    );
    Ok(())
}

#[tokio::test]
async fn managed_rg_globs_limits_and_discovery_work_in_serial_and_parallel()
-> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    fs::create_dir(root.path().join(".git"))?;
    for file in [
        "keep.rs",
        "drop.txt",
        "ignored.rs",
        "vcs.rs",
        ".hidden.rs",
        ".git/secret.rs",
    ] {
        fs::write(root.path().join(file), "needle\nneedle\n")?;
    }
    fs::write(root.path().join(".ignore"), "ignored.rs\n")?;
    fs::write(root.path().join(".gitignore"), "vcs.rs\n")?;
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["--iglob", "*.RS", "-g", "!keep.rs"],
            &[".hidden.rs", "ignored.rs", "vcs.rs"],
        ),
        (
            &["-g", "!keep.rs", "--iglob", "*.RS"],
            &[".hidden.rs", "ignored.rs", "keep.rs", "vcs.rs"],
        ),
        (&["--no-ignore-dot"], &["drop.txt", "ignored.rs", "keep.rs"]),
        (&["--no-ignore-vcs"], &["drop.txt", "keep.rs", "vcs.rs"]),
        (&["-u"], &["drop.txt", "ignored.rs", "keep.rs", "vcs.rs"]),
        (
            &["-uu"],
            &[".hidden.rs", "drop.txt", "ignored.rs", "keep.rs", "vcs.rs"],
        ),
        (
            &["--glob-case-insensitive", "-g", "*.RS"],
            &[".hidden.rs", "ignored.rs", "keep.rs", "vcs.rs"],
        ),
    ];
    for threads in ["1", "2"] {
        for (args, expected) in cases {
            let mut args = args.to_vec();
            args.extend(["-j", threads, "-m1", "needle", "."]);
            let reply = search_args(root.path(), &args).await?;
            let actual = reply
                .items
                .iter()
                .map(|item| item.relative_path.as_path())
                .collect::<Vec<_>>();
            let expected = expected
                .iter()
                .map(std::path::Path::new)
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "{args:?}");
            assert!(
                reply
                    .diagnostics
                    .rg
                    .as_ref()
                    .expect("rg diagnostics")
                    .truncated
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn managed_rg_handles_crlf_nul_and_multiple_multiline_hits() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    fs::write(root.path().join("crlf.txt"), "needle\r\nother\r\n")?;
    let reply = search_args(root.path(), &["--crlf", "-x", "needle", "crlf.txt"]).await?;
    assert_eq!(start_lines(&reply), [1]);
    assert_eq!(reply.items[0].content, "needle");
    fs::write(root.path().join("nul.txt"), "before\0needle\nafter\n")?;
    let reply = search_args(root.path(), &["-a", "needle", "nul.txt"]).await?;
    assert_eq!(start_lines(&reply), [1]);
    fs::write(
        root.path().join("multi.txt"),
        "first\nsecond\nfirst\nsecond\nlast\n",
    )?;
    let reply = search_args(root.path(), &["-U", "first\\nsecond", "multi.txt"]).await?;
    assert_eq!(start_lines(&reply), [1, 3]);
    let reply = search_args(root.path(), &["-Uv", "first\\nsecond", "multi.txt"]).await?;
    assert_eq!(start_lines(&reply), [5]);
    fs::write(root.path().join("overlap.txt"), "a a\na a\n")?;
    let reply = search_args(root.path(), &["-Um1", "a|\\n", "overlap.txt"]).await?;
    assert_eq!(start_lines(&reply), [1]);
    assert_eq!(reply.items[0].content, "a a");
    assert_eq!(
        reply.coverage,
        zg_engine::api::context::result::ContextCoverage::RgTruncated
    );
    Ok(())
}

#[tokio::test]
async fn managed_rg_explicit_ignore_files_can_be_disabled() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    fs::write(root.path().join("sample.txt"), "needle\n")?;
    fs::write(root.path().join(".custom-ignore"), "sample.txt\n")?;
    let reply = search_args(
        root.path(),
        &["--ignore-file", ".custom-ignore", "needle", "."],
    )
    .await?;
    assert!(reply.items.is_empty());
    let reply = search_args(
        root.path(),
        &[
            "--no-ignore-files",
            "--ignore-file",
            ".custom-ignore",
            "needle",
            ".",
        ],
    )
    .await?;
    assert_eq!(reply.items.len(), 1);
    let reply = search_args(
        root.path(),
        &[
            "--no-ignore-files",
            "--ignore-files",
            "--ignore-file",
            ".custom-ignore",
            "needle",
            ".",
        ],
    )
    .await?;
    assert!(reply.items.is_empty());
    Ok(())
}
