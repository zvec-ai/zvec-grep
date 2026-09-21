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
    assert_eq!(main, stdout(&["help"]));
    assert!(main.starts_with("zvec-grep 0.0.1\n\nUsage:\n  zg <command> [options]\n"));
    assert!(main.contains("Run zg help environment for all variables"));
}

#[test]
fn command_help_and_topic_aliases_are_identical() {
    assert_eq!(stdout(&["query", "--help"]), stdout(&["help", "query"]));
    assert_eq!(
        stdout(&["query", "ignored", "--help", "also-ignored"]),
        stdout(&["help", "query"])
    );
    assert_eq!(stdout(&["help", "environment"]), stdout(&["help", "env"]));
}

#[test]
fn every_documented_help_topic_is_available() {
    for topic in [
        "query",
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
        let output = stdout(&["help", topic]);
        assert!(output.starts_with("Usage:\n"), "missing usage for {topic}");
    }
}

#[test]
fn models_help_uses_the_rust_transformers_backend_name() {
    let help = stdout(&["help", "models"]);
    assert!(help.contains("local/all-minilm-l6-v2"));
    assert!(help.contains("  transformers\n"));
    assert!(!help.contains("transformers-js"));
}

#[test]
fn index_help_describes_the_optional_workspace_name() {
    let help = stdout(&["index", "--help"]);
    assert!(help.contains("--name <NAME>"));
    assert!(help.contains("defaults to root directory name"));
    assert!(help.contains("Scan rules:"));
    assert!(!help.contains("--type"));
    assert!(!help.contains("--category"));
}

#[test]
fn version_aliases_match_typescript_contract() {
    let expected = "0.0.1\n";
    assert_eq!(stdout(&["version"]), expected);
    assert_eq!(stdout(&["version", "-v"]), expected);
    assert_eq!(stdout(&["version", "--version"]), expected);
    assert_eq!(stdout(&["-v"]), expected);
    assert_eq!(stdout(&["--version"]), expected);
}

#[test]
fn unknown_help_topic_is_reported_as_a_user_error() {
    let output = zg(&["help", "not-a-topic"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).expect("error should be UTF-8"),
        "Error: Unknown help topic: not-a-topic\n"
    );
}
