//! Shared, static human-facing messages. Data streams keep their own formats.

use crate::{
    ColorMode, OutputOptions,
    theme::{StatusTheme, clean, unicode_supported},
};
use std::io::{self, Write};
use zg_engine::ErrorReport;

#[derive(Clone, Copy)]
pub enum MessageKind {
    Success,
    Warning,
    Error,
    Info,
}

/// Writes a status heading and indented details, without terminal cursor control.
///
/// # Errors
/// Returns an output error.
pub fn write_message(
    mut writer: impl Write,
    kind: MessageKind,
    title: &str,
    details: &[&str],
    color: ColorMode,
    terminal: bool,
) -> io::Result<()> {
    let theme = StatusTheme::new(color, terminal);
    let title = clean(title);
    match kind {
        MessageKind::Warning => writeln!(writer, "{} {title}", theme.warning("Warning:"))?,
        MessageKind::Error => writeln!(writer, "{} {title}", theme.danger("Error:"))?,
        MessageKind::Success | MessageKind::Info if terminal => {
            let glyph = if matches!(kind, MessageKind::Success) {
                theme.success(if unicode_supported() { "✓" } else { "+" })
            } else {
                theme.muted(if unicode_supported() { "·" } else { "-" })
            };
            writeln!(writer, "{glyph} {title}")?;
        }
        MessageKind::Success | MessageKind::Info => writeln!(writer, "{title}")?,
    }
    for detail in details {
        writeln!(writer, "  {}", theme.muted(&clean(detail)))?;
    }
    Ok(())
}

/// Renders a structured error with actionable help; debug and logs retain its code and origin.
///
/// # Errors
/// Returns an output error.
pub fn write_error_report(
    mut writer: impl Write,
    report: &ErrorReport,
    retryable: Option<bool>,
    options: OutputOptions,
    terminal: bool,
) -> io::Result<()> {
    if !terminal || options.debug {
        let diagnostic = crate::format_error_report(report);
        if terminal {
            for line in diagnostic.lines() {
                writeln!(writer, "{}", clean(line))?;
            }
        } else {
            writeln!(writer, "{diagnostic}")?;
        }
        if let Some(retryable) = retryable {
            writeln!(writer, "retryable: {retryable}")?;
        }
        return Ok(());
    }
    write_error_message(&mut writer, &report.message, options, terminal)?;
    if let Some(help) = &report.help {
        writeln!(writer, "  {}", clean(help))?;
    }
    Ok(())
}

/// Keeps arbitrary error causes intact, with one heading and consistent indentation.
///
/// # Errors
/// Returns an output error.
pub fn write_error_message(
    mut writer: impl Write,
    message: &str,
    options: OutputOptions,
    terminal: bool,
) -> io::Result<()> {
    if !terminal {
        return writeln!(writer, "Error: {message}");
    }
    let mut lines = message.trim().lines();
    let title = lines
        .next()
        .unwrap_or("Command failed")
        .trim_start_matches("error: ")
        .trim_start_matches("Error: ");
    write_message(
        &mut writer,
        MessageKind::Error,
        title,
        &[],
        options.color,
        terminal,
    )?;
    for line in lines {
        writeln!(writer, "  {}", clean(line))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zg_engine::EngineError;

    #[test]
    fn error_reports_preserve_engine_text_and_explicit_help() {
        let manifest_message = "workspace index '/repo/.zvec-grep/index' has no manifest; rebuild the index with `zg --index --rebuild`";
        for report in [
            EngineError::not_found("Index metadata is unavailable")
                .with_help("Restore the metadata from a backup.")
                .report(),
            EngineError::storage_failure(manifest_message)
                .with_help("Check the workspace location before rebuilding.")
                .report(),
            EngineError::storage_failure(manifest_message).report(),
        ] {
            for (terminal, debug) in [(true, false), (true, true), (false, false), (false, true)] {
                let mut bytes = Vec::new();
                write_error_report(
                    &mut bytes,
                    &report,
                    Some(false),
                    OutputOptions {
                        debug,
                        color: ColorMode::Never,
                        ..Default::default()
                    },
                    terminal,
                )
                .expect("render");
                let text = String::from_utf8(bytes).expect("text");
                let expected = if terminal && !debug {
                    let help = report
                        .help
                        .as_ref()
                        .map_or_else(String::new, |help| format!("  {help}\n"));
                    format!("Error: {}\n{help}", report.message)
                } else {
                    format!(
                        "{}\nretryable: false\n",
                        crate::format_error_report(&report)
                    )
                };
                assert_eq!(text, expected);
            }
        }
    }

    #[test]
    fn arbitrary_storage_errors_keep_cause_without_inventing_recovery() {
        let report = EngineError::storage_failure("disk full\ncaused by: volume\x1b[2J").report();
        let mut bytes = Vec::new();
        write_error_report(
            &mut bytes,
            &report,
            None,
            OutputOptions {
                color: ColorMode::Never,
                ..Default::default()
            },
            true,
        )
        .expect("render");
        let text = String::from_utf8(bytes).expect("text");
        assert!(text.contains("disk full\n  caused by: volume [2J"));
        assert!(!text.contains("rebuild"));
        assert!(!text.contains('\x1b'));
    }
}
