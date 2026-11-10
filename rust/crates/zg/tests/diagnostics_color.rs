use std::process::{Command, Output};

#[cfg(unix)]
#[allow(dead_code)]
#[path = "support/terminal.rs"]
mod terminal;

fn command(arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zg"));
    command.args(arguments).env("TERM", "xterm-256color");
    for variable in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE", "FORCE_COLOR"] {
        command.env_remove(variable);
    }
    command
}

fn assert_diagnostic(output: &Output, colored: bool) {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("Error"), "{error}");
    assert_eq!(error.contains("\x1b["), colored, "{error:?}");
}

#[test]
fn removed_no_color_alias_is_rejected_with_its_replacement() {
    for arguments in [
        vec!["needle", "--no-color"],
        vec!["--index", "--no-color"],
        vec!["--status", "--no-color"],
        vec!["--auth", "status", "--no-color"],
    ] {
        let output = command(&arguments).output().expect("CLI diagnostic");
        assert_diagnostic(&output, false);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("--no-color"), "{error}");
        assert!(error.contains("--color never"), "{error}");
    }
}

#[test]
fn parse_errors_respect_explicit_colors_and_pipe_detection() {
    for (arguments, no_color, forced_environment, colored) in [
        (vec!["--index", "--build"], None, false, false),
        (vec!["--index", "--build"], None, true, false),
        (
            vec!["--index", "--build", "--color", "always"],
            Some("1"),
            false,
            true,
        ),
        (
            vec!["--index", "--color=always", "--build"],
            Some(""),
            false,
            true,
        ),
        (
            vec!["--index", "--color", "never", "--build"],
            None,
            true,
            false,
        ),
        (
            vec!["--index", "--build", "--", "--color", "always"],
            None,
            false,
            false,
        ),
        (
            vec!["--fts", "--color", "always", "--no-color"],
            None,
            false,
            false,
        ),
    ] {
        let mut command = command(&arguments);
        if let Some(value) = no_color {
            command.env("NO_COLOR", value);
        }
        if forced_environment {
            command.env("CLICOLOR_FORCE", "1").env("FORCE_COLOR", "1");
        }
        let output = command.output().expect("CLI diagnostic");
        assert_diagnostic(&output, colored);
    }
}

#[cfg(unix)]
#[test]
fn terminal_parse_errors_honor_no_color_and_explicit_overrides() {
    for (arguments, no_color, colored) in [
        (vec!["--index", "--build"], None, true),
        (vec!["--index", "--build"], Some("1"), false),
        (vec!["--index", "--build"], Some(""), false),
        (vec!["--index", "--color", "never", "--build"], None, false),
        (
            vec!["--index", "--color", "always", "--build"],
            Some("1"),
            true,
        ),
    ] {
        let mut command = command(&arguments);
        if let Some(value) = no_color {
            command.env("NO_COLOR", value);
        }
        let output = terminal::run(command, &[]);
        assert_diagnostic(&output, colored);
    }
}
