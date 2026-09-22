use std::process::{Command, Output};

fn run(root: &std::path::Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zg"))
        .args(arguments)
        .current_dir(root)
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("ZVEC_GREP_HOME", root.join("state"))
        .env("NO_COLOR", "1")
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
                arguments.extend(["--limit", "3", "--compact"]);
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
