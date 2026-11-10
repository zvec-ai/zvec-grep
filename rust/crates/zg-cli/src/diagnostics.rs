use std::{ffi::OsString, fmt::Write as _};

use clap::{
    ColorChoice, CommandFactory,
    builder::StyledStr,
    error::{ContextKind, ContextValue, ErrorFormatter, ErrorKind},
};

/// Recovers the requested color policy when argument parsing has failed.
#[must_use]
pub fn diagnostic_color_for_args(arguments: &[OsString]) -> crate::ColorMode {
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            break;
        }
        let value = if argument == "--color" {
            arguments.next().and_then(|value| value.to_str())
        } else {
            let text = argument.to_string_lossy();
            if crate::query_option_with_value(&text)
                || crate::managed_rg::takes_separate_value(&text)
                || crate::management_option_with_value(&text)
            {
                arguments.next();
            }
            argument
                .to_str()
                .and_then(|value| value.strip_prefix("--color="))
        };
        match value {
            Some("always") => return crate::ColorMode::Always,
            Some("never") => return crate::ColorMode::Never,
            Some("auto") => return crate::ColorMode::Auto,
            Some("--") => break,
            _ => {}
        }
    }
    crate::ColorMode::Auto
}

/// Prints a parse diagnostic with the same color policy as other CLI output.
pub fn exit_parse_error(error: &clap::Error, color: crate::ColorMode) -> ! {
    presentation_error(error, color).exit()
}

struct DiagnosticFormatter;

impl ErrorFormatter for DiagnosticFormatter {
    fn format_error(error: &clap::error::Error<Self>) -> StyledStr {
        match error.get(ContextKind::Custom) {
            Some(ContextValue::StyledStr(message)) => message.clone(),
            _ => StyledStr::new(),
        }
    }
}

fn presentation_error(
    error: &clap::Error,
    color: crate::ColorMode,
) -> clap::error::Error<DiagnosticFormatter> {
    let message = error.render().ansi().to_string();
    let message = if matches!(
        error.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            | ErrorKind::DisplayVersion
    ) {
        message
    } else {
        capitalize_labels(&message)
    };
    let terminal = if error.use_stderr() {
        crate::stderr_supports_ansi()
    } else {
        crate::stdout_supports_ansi()
    };
    let color = if crate::theme::StatusTheme::new(color, terminal).color {
        ColorChoice::Always
    } else {
        ColorChoice::Never
    };
    let command = crate::Cli::command().color(color);
    let mut presentation = clap::error::Error::new(error.kind()).with_cmd(&command);
    presentation.insert(ContextKind::Custom, ContextValue::StyledStr(message.into()));
    presentation
}

fn capitalize_labels(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    for line in message.split_inclusive('\n') {
        let mut start = 0;
        loop {
            let rest = &line[start..];
            if rest.starts_with([' ', '\t']) {
                start += 1;
            } else if let Some(escape) = rest.strip_prefix("\x1b[")
                && let Some(end) = escape.find('m')
            {
                start += 2 + end + 1;
            } else {
                break;
            }
        }
        output.push_str(&line[..start]);
        let rest = &line[start..];
        if ["error:", "warning:", "tip:"]
            .iter()
            .any(|label| rest.starts_with(label))
        {
            output.push(char::from(rest.as_bytes()[0]).to_ascii_uppercase());
            output.push_str(&rest[1..]);
        } else {
            output.push_str(rest);
        }
    }
    output
}

/// Formats engine diagnostics for the CLI without changing the engine error contract.
#[must_use]
pub fn format_error_report(report: &zg_engine::ErrorReport) -> String {
    let mut output = format!("Error[{}]: {}", report.code, report.message);
    if let Some(help) = &report.help {
        let _ = write!(output, "\nHelp: {help}");
    }
    let _ = write!(output, "\nError location: {}", report.origin);
    if let Some(reported_at) = &report.reported_at {
        let _ = write!(output, "\nReporting location: {reported_at}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_diagnostic_preserves_styles_payload_usage_and_exit_status() {
        let error = crate::Cli::try_parse_from(["zg", "--index", "--build"])
            .expect_err("misspelled rebuild option");
        let original = error.render().ansi().to_string();
        let presentation = presentation_error(&error, crate::ColorMode::Always);
        let rendered = presentation.render();
        let plain = rendered.to_string();
        assert!(
            plain.starts_with("Error: unexpected argument '--build' found"),
            "{plain}"
        );
        assert!(
            plain.contains("Tip: a similar argument exists: '--rebuild'"),
            "{plain}"
        );
        assert!(
            plain.contains("Usage: zg --index --rebuild [ROOT]"),
            "{plain}"
        );
        assert_eq!(presentation.exit_code(), error.exit_code());
        assert_eq!(presentation.use_stderr(), error.use_stderr());
        assert_eq!(
            rendered.ansi().to_string().matches('\x1b').count(),
            original.matches('\x1b').count()
        );
        let raw = clap::Error::raw(
            ErrorKind::InvalidValue,
            "invalid value 'error: warning: tip:'",
        );
        assert_eq!(
            presentation_error(&raw, crate::ColorMode::Never).to_string(),
            "Error: invalid value 'error: warning: tip:'"
        );
    }

    #[test]
    fn help_output_is_not_rewritten() {
        let error = clap::Command::new("fixture")
            .after_help("tip: user-supplied help text")
            .try_get_matches_from(["fixture", "--help"])
            .expect_err("help requested");
        let presentation = presentation_error(&error, crate::ColorMode::Never);
        assert_eq!(presentation.to_string(), error.to_string());
        assert_eq!(presentation.exit_code(), 0);
        assert!(!presentation.use_stderr());
    }

    #[test]
    fn diagnostic_color_ignores_literal_queries_and_invalid_values() {
        for (arguments, expected) in [
            (
                vec!["--color", "always", "--build"],
                crate::ColorMode::Always,
            ),
            (vec!["--build", "--color=never"], crate::ColorMode::Never),
            (vec!["--color=invalid"], crate::ColorMode::Auto),
            (vec!["--color"], crate::ColorMode::Auto),
            (vec!["--", "--color", "always"], crate::ColorMode::Auto),
            (vec!["--", "--color=always"], crate::ColorMode::Auto),
            (vec!["--fts", "--color", "always"], crate::ColorMode::Auto),
            (vec!["--hybrid=--color", "always"], crate::ColorMode::Auto),
            (
                vec!["--rg", "-e", "--color", "always"],
                crate::ColorMode::Auto,
            ),
            (vec!["--api-key", "--color=always"], crate::ColorMode::Auto),
            (
                vec!["--color", "--", "--color", "always"],
                crate::ColorMode::Auto,
            ),
            (
                vec!["--color", "never", "--", "--color", "always"],
                crate::ColorMode::Never,
            ),
        ] {
            let arguments = arguments
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>();
            assert_eq!(
                diagnostic_color_for_args(&arguments),
                expected,
                "{arguments:?}"
            );
        }
    }
}
