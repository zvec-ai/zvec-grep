use std::process::{Command, Output};

fn zg(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zg"))
        .args(arguments)
        .output()
        .expect("zg should run")
}

fn stdout(arguments: &[&str]) -> String {
    let output = zg(arguments);
    assert!(
        output.status.success(),
        "zg {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("help output should be UTF-8")
}

#[test]
fn main_help_aliases_have_one_stable_surface() {
    let main = stdout(&[]);
    assert_eq!(main, stdout(&["-h"]));
    assert_eq!(main, stdout(&["--help"]));
    assert!(main.starts_with("zvec-grep 0.0.1\n\nUsage:\n  zg <query> [options]\n"));
    assert!(main.contains("Run zg --help environment for all variables"));
    assert!(main.contains("--compact"));
    assert!(!main.contains("--human"));
}

#[test]
fn command_help_and_topic_aliases_are_identical() {
    assert_eq!(stdout(&["--index", "--help"]), stdout(&["--help", "index"]));
    assert_eq!(stdout(&["-h", "search"]), stdout(&["--help", "search"]));
    assert_eq!(
        stdout(&["--help", "environment"]),
        stdout(&["--help", "env"])
    );
}

#[test]
fn every_documented_help_topic_is_available() {
    for topic in [
        "search",
        "index",
        "status",
        "config",
        "auth",
        "server",
        "install",
        "uninstall",
        "help",
        "models",
        "file-types",
        "environment",
        "env",
        "version",
    ] {
        let output = stdout(&["--help", topic]);
        assert!(output.starts_with("Usage:\n"), "missing usage for {topic}");
    }
}

#[test]
fn search_help_describes_terminal_and_compact_defaults() {
    let help = stdout(&["--help", "search"]);
    assert!(help.starts_with("Usage:\n  zg <query> [options]\n"));
    assert!(help.contains("--compact"));
    assert!(help.contains("full on TTY, none in compact mode"));
    assert!(!help.contains("--human"));
    assert!(!help.contains("zg query"));
}

#[test]
fn models_help_uses_the_rust_transformers_backend_name() {
    let help = stdout(&["--help", "models"]);
    assert!(help.contains("local/all-minilm-l6-v2"));
    assert!(help.contains("  transformers\n"));
    assert!(!help.contains("transformers-js"));
}

#[test]
fn index_help_describes_the_optional_workspace_name() {
    let help = stdout(&["--index", "--help"]);
    assert!(help.contains("--name <NAME>"));
    assert!(help.contains("defaults to root directory name"));
    assert!(help.contains("Scan rules:"));
    assert!(!help.contains("--type"));
    assert!(!help.contains("--category"));
}

#[test]
fn index_help_distinguishes_explicit_indexing_from_automatic_search_indexing() {
    let help = stdout(&["--help", "index"]);
    assert!(
        help.contains("Explicit zg --index requires --embedding"),
        "{help}"
    );
    assert!(
        help.contains("Search automatically creates a missing index"),
        "{help}"
    );
    assert!(help.contains("local/potion-code-16m-v2"), "{help}");
    assert!(help.contains("never a remote model"), "{help}");
}

#[test]
fn version_flags_print_the_installed_version() {
    let expected = "0.0.1\n";
    assert_eq!(stdout(&["-v"]), expected);
    assert_eq!(stdout(&["--version"]), expected);
}

#[test]
fn unknown_help_topics_including_old_query_alias_are_user_errors() {
    for topic in ["not-a-topic", "query"] {
        let output = zg(&["--help", topic]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            String::from_utf8(output.stderr).expect("error should be UTF-8"),
            format!("Error: Unknown help topic: {topic}\n")
        );
    }
}
