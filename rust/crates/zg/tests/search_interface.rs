use std::process::{Command, Output};

fn run(root: &std::path::Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zg"))
        .args(arguments)
        .current_dir(root)
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("ZVEC_GREP_HOME", root.join("state"))
        .env("NO_COLOR", "1")
        .env("ZVEC_GREP_EMBEDDING", "unsupported/model")
        .env_remove("ZVEC_GREP_MODE")
        .output()
        .expect("run zg")
}

#[test]
fn command_shaped_queries_warn_once_and_still_search() {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::write(
        root.path().join("source.txt"),
        "query\nsearch\nindex\nstatus\ninstall\nuninstall\nconfig\nauth\nserver\nhelp\nversion\n",
    )
    .expect("fixture");
    for word in [
        "query",
        "search",
        "index",
        "status",
        "install",
        "uninstall",
        "config",
        "auth",
        "server",
        "help",
        "version",
    ] {
        let output = run(root.path(), &[word, "--rg", "source.txt"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{word}: {stderr}");
        assert_eq!(stderr.matches("warning:").count(), 1, "{stderr}");
        assert!(stderr.contains("literal word"), "{stderr}");
        assert!(stdout.contains("source.txt"), "{stdout}");
        assert!(stdout.contains(word), "{stdout}");
        assert!(!stdout.contains("warning:"), "{stdout}");
    }
    assert!(!root.path().join(".zvec-grep").exists());
}

#[test]
fn literal_flags_and_command_words_do_not_warn_or_run_actions() {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::write(
        root.path().join("source.txt"),
        "query\n--index\n--human\n--help\n--\n",
    )
    .expect("fixture");
    for word in ["query", "--index", "--human", "--help", "--"] {
        for pattern_option in ["--", "-e", "-Fe", "--regexp"] {
            let mut arguments = vec!["--rg", pattern_option, word, "source.txt"];
            if pattern_option != "--" {
                arguments.extend(["--limit", "3"]);
            }
            let output = run(root.path(), &arguments);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "{word}: {stderr}");
            assert!(!stderr.contains("warning:"), "{stderr}");
            assert!(stdout.contains("source.txt"), "{stdout}");
            assert!(stdout.contains(word), "{stdout}");
        }
    }
    assert!(!root.path().join(".zvec-grep").exists());
}

#[test]
fn removed_flags_and_ambiguous_actions_fail_without_side_effects() {
    let root = tempfile::tempdir().expect("workspace");
    for arguments in [
        vec!["index", "--drop", "--yes"],
        vec!["--human", "needle"],
        vec!["--index", "--status"],
    ] {
        let output = run(root.path(), &arguments);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        if arguments[0] == "index" {
            assert!(stderr.contains("no longer runs an action"), "{stderr}");
        }
        if arguments[0] == "--human" {
            assert!(stderr.contains("--human has been removed"), "{stderr}");
        }
    }
    assert!(!root.path().join(".zvec-grep").exists());
}

const PATH_LOOKUP_NOTICE: &str =
    "warning: automatically performing path lookup; use --hybrid to force hybrid search.\n";
const TEXT_FALLBACK_NOTICE: &str =
    "warning: no matching file paths; searching current file contents literally (rg).\n";

#[test]
fn file_lookup_prints_ranked_path_matches_in_every_client_mode_without_a_daemon() {
    let root = tempfile::tempdir().expect("workspace");
    for file in [
        "auth.ts",
        "src/auth.ts",
        "otherauth.ts",
        "node_modules/auth.ts",
        "ignored/auth.ts",
    ] {
        let path = root.path().join(file);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directories");
        std::fs::write(path, b"").expect("empty file");
    }
    std::fs::write(root.path().join(".gitignore"), "ignored/\n").expect("ignore rules");
    for mode in ["direct", "server", "auto"] {
        for (query, filter, expected) in [
            (
                "auth.ts",
                None,
                vec!["auth.ts", "node_modules/auth.ts", "src/auth.ts"],
            ),
            ("src/auth.ts", None, vec!["src/auth.ts"]),
            (".\\src\\auth.ts", None, vec!["src/auth.ts"]),
            ("auth.ts", Some("src/**"), vec!["src/auth.ts"]),
        ] {
            let mut args = vec![query, "--mode", mode, "--no-color"];
            if let Some(filter) = filter {
                args.extend(["-g", filter, "--limit", "1"]);
            }
            let output = run(root.path(), &args);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{args:?}: {stderr}");
            assert_eq!(stderr, PATH_LOOKUP_NOTICE);
            let expected: Vec<_> = expected
                .iter()
                .enumerate()
                .map(|(index, path)| format!("#{} matchedBy=path {path}", index + 1))
                .collect();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let actual: Vec<_> = stdout.lines().collect();
            assert_eq!(actual, expected, "{args:?}");
        }
    }
    assert!(!root.path().join(".zvec-grep").exists());
    assert!(!root.path().join("state").exists());
}

#[test]
fn explicit_routes_continue_to_indexed_search() {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::write(root.path().join("auth.ts"), "export const auth = true;").expect("source");
    let home = root.path().join(".zvec-grep");
    std::fs::create_dir(&home).expect("workspace home");
    std::fs::write(
        home.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({
            "sourceRoot": {
                "recordedPath": std::fs::canonicalize(root.path()).expect("source root").to_string_lossy(),
                "recordedUsername": std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).ok(),
            },
            "scan": {},
            "createdTime": 0,
            "updatedTime": 0,
            "index": { "state": "disabled" },
        }))
        .expect("manifest json"),
    )
    .expect("disabled manifest");
    for mut args in [
        vec!["authentication"],
        vec!["find auth.ts"],
        vec!["auth.ts", "login"],
        vec!["--hybrid", "auth.ts"],
        vec!["--fts", "auth.ts"],
        vec!["--vector", "auth.ts"],
        vec!["auth.ts", "--prefer-symbol"],
    ] {
        args.extend(["--mode", "direct"]);
        let output = run(root.path(), &args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}: {stdout}");
        assert!(!stdout.contains("auth.ts"), "{stdout}");
        assert!(!stderr.contains("No index found"), "{stderr}");
        assert!(
            stderr.contains("ZG.ENGINE.UNSUPPORTED"),
            "{args:?}: {stderr}"
        );
        assert!(
            stderr.contains("workspace indexing is disabled"),
            "{args:?}: {stderr}"
        );
        assert!(
            !stderr.contains("automatically performing path lookup"),
            "{stderr}"
        );
        assert!(!stderr.contains(TEXT_FALLBACK_NOTICE), "{stderr}");
    }
}

#[test]
fn path_queries_use_current_literal_text_and_truthful_no_matches_without_an_index() {
    let root = tempfile::tempdir().expect("workspace");
    for (file, content) in [
        (
            "src/notes.md",
            "Import src/auth.ts to enable authentication.\n",
        ),
        ("excluded/notes.md", "src/auth.ts is excluded.\n"),
        ("ignored/notes.md", "src/auth.ts is ignored.\n"),
        ("src/decoy.md", "src/authXts is a different string.\n"),
    ] {
        let path = root.path().join(file);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directories");
        std::fs::write(path, content).expect("source text");
    }
    std::fs::write(root.path().join(".gitignore"), "ignored/\n").expect("ignore rules");
    for mode in ["direct", "server", "auto"] {
        for query in ["missing.ts", "config.timeout", "v1.2"] {
            let output = run(root.path(), &[query, "--mode", mode]);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{mode}: {stderr}");
            assert_eq!(String::from_utf8_lossy(&output.stdout), "No matches.\n");
            assert_eq!(
                stderr,
                format!("{PATH_LOOKUP_NOTICE}{TEXT_FALLBACK_NOTICE}")
            );
        }
        for query in ["src/auth.ts", "auth.ts"] {
            let output = run(
                root.path(),
                &[query, "--mode", mode, "-g", "src/**", "--limit", "1"],
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "{mode}: {stderr}");
            assert!(stdout.contains("src/notes.md"), "{stdout}");
            assert!(stdout.contains("Import src/auth.ts"), "{stdout}");
            assert!(!stdout.contains("decoy.md"), "{stdout}");
            assert!(!stdout.contains("excluded"), "{stdout}");
            assert!(!stdout.contains("ignored"), "{stdout}");
            assert_eq!(
                stderr,
                format!("{PATH_LOOKUP_NOTICE}{TEXT_FALLBACK_NOTICE}")
            );
        }
    }
    assert!(!root.path().join(".zvec-grep").exists());
    assert!(!root.path().join("state").exists());
}

#[test]
fn path_lookup_reports_file_and_text_truncation_without_indexing_in_all_modes() {
    let root = tempfile::tempdir().expect("workspace");
    let contract_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compat/cli/path-lookup.json");
    let contract =
        zg_testkit::load_cli_case(&contract_path).expect("path lookup compatibility fixture");
    for file in ["a/auth.ts", "b/auth.ts"] {
        let path = root.path().join(file);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, b"\xff\x00").expect("binary file names remain searchable");
    }
    for file in ["a.txt", "b.txt"] {
        std::fs::write(root.path().join(file), "before\nImport missing.ts\nafter\n")
            .expect("text fixture");
    }
    for mode in ["direct", "server", "auto"] {
        let files = run(root.path(), &["auth.ts", "--mode", mode, "--limit", "1"]);
        assert_eq!(files.status.code(), Some(contract.expected.exit_code));
        assert_eq!(
            String::from_utf8_lossy(&files.stdout),
            contract.expected.stdout
        );
        assert_eq!(
            String::from_utf8_lossy(&files.stderr),
            contract.expected.stderr
        );
        let text = run(root.path(), &["missing.ts", "--mode", mode, "--limit", "1"]);
        assert!(text.status.success(), "{mode}: {:?}", text.stderr);
        assert_eq!(
            String::from_utf8_lossy(&text.stdout),
            "a.txt\n  1-\tbefore\n  2:\tImport missing.ts\n  3-\tafter\n"
        );
        let stderr = String::from_utf8_lossy(&text.stderr);
        assert!(
            stderr.starts_with(&format!("{PATH_LOOKUP_NOTICE}{TEXT_FALLBACK_NOTICE}")),
            "{stderr}"
        );
        assert_eq!(stderr.matches(PATH_LOOKUP_NOTICE).count(), 1);
        assert_eq!(stderr.matches(TEXT_FALLBACK_NOTICE).count(), 1);
        assert!(stderr.contains("coverage is incomplete"), "{stderr}");
        assert!(!stderr.contains("No index found"), "{stderr}");
    }
    assert!(!root.path().join(".zvec-grep").exists());
    assert!(!root.path().join("state").exists());
}

#[test]
fn path_lookup_deduplicates_entities_beyond_100_files_in_every_mode() {
    let root = tempfile::tempdir().expect("workspace");
    for index in 0..101 {
        let repeated = if index == 100 { "  // auth.ts\n" } else { "" };
        std::fs::write(
            root.path().join(format!("{index:03}.ts")),
            format!(
                "function load() {{\n  // auth.ts\n  // before\n  // middle\n  // after\n{repeated}}}\n"
            ),
        )
        .expect("source references");
    }
    for mode in ["direct", "server", "auto"] {
        let output = run(root.path(), &["auth.ts", "--mode", mode, "--limit", "101"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{mode}: {stderr}");
        assert_eq!(
            stderr,
            format!("{PATH_LOOKUP_NOTICE}{TEXT_FALLBACK_NOTICE}"),
            "{mode}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.matches("[function load]").count(),
            101,
            "{mode}: {stdout}"
        );
        assert!(stdout.contains("100.ts\n"), "{mode}: {stdout}");

        let output = run(root.path(), &["auth.ts", "--mode", mode, "--limit", "100"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{mode}: {stderr}");
        assert!(
            stderr.starts_with(&format!("{PATH_LOOKUP_NOTICE}{TEXT_FALLBACK_NOTICE}")),
            "{mode}: {stderr}"
        );
        assert_eq!(stderr.matches(TEXT_FALLBACK_NOTICE).count(), 1);
        assert!(
            stderr.contains("coverage is incomplete"),
            "{mode}: {stderr}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout)
                .matches("[function load]")
                .count(),
            100
        );
    }
    assert!(!root.path().join(".zvec-grep").exists());
    assert!(!root.path().join("state").exists());
}

#[test]
fn path_lookup_smart_case_highlights_only_literal_matches_in_every_mode() {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::write(root.path().join("refs.txt"), "auth.ts\nAUTH.ts\nAUTHXts\n")
        .expect("source references");
    for mode in ["direct", "server", "auto"] {
        let output = run(
            root.path(),
            &["AUTH.ts", "--mode", mode, "--color", "always"],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{mode}: {stderr}");
        assert_eq!(
            stderr,
            format!("{PATH_LOOKUP_NOTICE}{TEXT_FALLBACK_NOTICE}")
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "refs.txt\n  1-\tauth.ts\n  2:\t\x1b[1;33mAUTH.ts\x1b[0m\n  3-\tAUTHXts\n",
            "{mode}"
        );
    }
    assert!(!root.path().join(".zvec-grep").exists());
    assert!(!root.path().join("state").exists());
}

#[test]
fn path_lookup_deduplicates_symbols_before_the_limit_and_highlights_only_matching_text() {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::write(
        root.path().join("refs.ts"),
        "export function load() {\n  const first = 'src/auth.ts';\n  const second = 'src/auth.ts';\n  return first + second;\n}\n\nexport function other() {\n  return 'src/auth.ts';\n}\n",
    )
    .expect("source references");
    for mode in ["direct", "server", "auto"] {
        let output = run(
            root.path(),
            &[
                "src/auth.ts",
                "--mode",
                mode,
                "--limit",
                "2",
                "--color",
                "always",
            ],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{mode}: {stderr}");
        assert_eq!(
            stderr,
            format!("{PATH_LOOKUP_NOTICE}{TEXT_FALLBACK_NOTICE}")
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.starts_with("refs.ts\n"), "{stdout}");
        assert_eq!(stdout.matches("[function load]").count(), 1, "{stdout}");
        assert_eq!(stdout.matches("[function other]").count(), 1, "{stdout}");
        assert!(stdout.contains("\x1b[1;33msrc/auth.ts\x1b[0m"), "{stdout}");
        assert!(!stdout.contains("\x1b[1;36m"), "{stdout}");
    }
    assert!(!root.path().join("state").exists());
}
