//! Presentation shared by index and authorization management commands.

use std::{
    io::{self, Write},
    path::Path,
};

use zg_engine::{
    api::index::{IndexResult, options::ScanRules},
    authorization::AuthorizationStatus,
};

use crate::{
    ColorMode, OutputOptions,
    status::scan_filters,
    theme::{StatusTheme, display_path, storage_path, write_status_field},
};

/// Writes a completed index reply without terminal styling.
///
/// # Errors
/// Returns the underlying writer error.
pub fn write_index_result(writer: impl Write, root: &Path, result: &IndexResult) -> io::Result<()> {
    write_index_with_options(writer, root, result, None, OutputOptions::default(), false)
}

/// Writes a completed index reply in the Node.js CLI's summary layout.
/// Pass the saved scan rules to describe the effective indexing scope.
///
/// # Errors
/// Returns the underlying writer error.
pub fn write_index_with_options(
    mut writer: impl Write,
    root: &Path,
    result: &IndexResult,
    scan: Option<&ScanRules>,
    options: OutputOptions,
    terminal: bool,
) -> io::Result<()> {
    let theme = StatusTheme::new(options.color, terminal);
    writeln!(writer, "{}", theme.accent("Workspace index"))?;
    let failed = format!("{} failed", result.files_failed);
    index_field(
        &mut writer,
        theme,
        "files",
        &format!(
            "{} scanned, {} added, {} modified, {} retried, {} unchanged, {} deleted, {}",
            result.files_scanned,
            theme.success(&result.files_added.to_string()),
            theme.warning(&result.files_modified.to_string()),
            theme.warning(&result.files_pending.to_string()),
            theme.muted(&result.files_unchanged.to_string()),
            theme.muted(&result.files_deleted.to_string()),
            if result.files_failed > 0 {
                theme.danger(&failed)
            } else {
                theme.success(&failed)
            },
        ),
    )?;
    index_field(
        &mut writer,
        theme,
        "entities",
        &result.entities_created.to_string(),
    )?;
    let duration_ms = result.duration_micros / 1_000;
    index_field(
        &mut writer,
        theme,
        "duration",
        &format!(
            "{} {}",
            format_duration(duration_ms),
            theme.muted(&format!("({duration_ms}ms)")),
        ),
    )?;
    let filters = scan.map(scan_filters).unwrap_or_default();
    let roots = if filters.is_empty() {
        root.display().to_string()
    } else {
        format!("{} ({filters})", root.display())
    };
    index_field(&mut writer, theme, "roots", &theme.path(&roots))?;
    for file in &result.failed_files {
        index_field(
            &mut writer,
            theme,
            "failed",
            &format!(
                "{}: {}",
                theme.path(&file.path.display().to_string()),
                theme.danger(&file.reason),
            ),
        )?;
    }
    if result.files_scanned == 0 {
        index_field(
            &mut writer,
            theme,
            "tip",
            &theme.warning(
                "No indexable files were found. Run `zg --help file-types` to review supported file types and indexing rules.",
            ),
        )?;
    }
    Ok(())
}

fn index_field(
    writer: &mut impl Write,
    theme: StatusTheme,
    label: &str,
    value: &str,
) -> io::Result<()> {
    writeln!(writer, "{}\t{value}", theme.label(label))
}

fn format_duration(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        return format!("{milliseconds}ms");
    }
    let seconds = milliseconds.saturating_add(500) / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {}s", seconds / 60, seconds % 60)
    }
}

/// Writes the result of an explicit index removal.
///
/// # Errors
/// Returns the underlying writer error.
pub fn write_index_drop_result(
    mut writer: impl Write,
    root: &Path,
    removed: bool,
) -> io::Result<()> {
    writeln!(
        writer,
        "{} {}",
        if removed {
            "Dropped index for"
        } else {
            "No index found for"
        },
        root.display(),
    )
}

/// Writes verified workspace authorization in the Node.js CLI's grouped layout.
///
/// # Errors
/// Returns the underlying writer error.
pub fn write_authorization_status(
    mut writer: impl Write,
    status: &AuthorizationStatus,
    color: ColorMode,
    terminal: bool,
) -> io::Result<()> {
    let theme = StatusTheme::new(color, terminal);
    writeln!(
        writer,
        "{}",
        if status.grants.is_empty() {
            theme.warning("○ Remote Embedding is not authorized")
        } else {
            theme.success("✓ Remote Embedding is authorized")
        },
    )?;
    writeln!(writer, "  {}", theme.path(&display_path(&status.root)))?;
    writeln!(writer)?;
    if status.grants.is_empty() {
        writeln!(writer, "{}", theme.accent("Run"))?;
        return writeln!(
            writer,
            "  {}",
            theme.path("zg --auth grant --capability embedding --scope workspace"),
        );
    }
    writeln!(writer, "{}", theme.accent("Authorization"))?;
    for (index, grant) in status.grants.iter().enumerate() {
        if index > 0 {
            writeln!(writer)?;
        }
        write_status_field(&mut writer, theme, "Scope", &[theme.success("Workspace")])?;
        write_status_field(
            &mut writer,
            theme,
            "Target",
            std::slice::from_ref(&grant.model),
        )?;
        write_status_field(
            &mut writer,
            theme,
            "Endpoint",
            std::slice::from_ref(&grant.endpoint_host),
        )?;
    }
    writeln!(writer)?;
    writeln!(writer, "{}", theme.accent("Storage"))?;
    write_status_field(
        &mut writer,
        theme,
        "Grant",
        &[theme.path(&storage_path(&status.path, &status.root))],
    )
}

/// Writes the result of revoking workspace authorization.
///
/// # Errors
/// Returns the underlying writer error.
pub fn write_authorization_revoke_result(
    mut writer: impl Write,
    count: Option<usize>,
) -> io::Result<()> {
    match count {
        Some(0) => writeln!(writer, "No Remote Embedding Workspace grants found."),
        Some(count) => writeln!(
            writer,
            "Revoked {count} Remote Embedding Workspace grant(s)."
        ),
        None => writeln!(writer, "Removed Remote Embedding Workspace authorization."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zg_engine::authorization::AuthorizationGrantStatus;

    #[test]
    fn index_result_distinguishes_failed_files_from_retried_work() {
        use zg_engine::api::info::result::FailedFile;
        let root = std::env::temp_dir().join("workspace");
        for failed in [0, 1] {
            let result = IndexResult {
                files_scanned: 2,
                // This counts retry candidates processed by the build, not remaining work.
                files_pending: 1,
                files_failed: failed,
                failed_files: if failed > 0 {
                    vec![FailedFile {
                        path: "broken.txt".into(),
                        reason: "prepare: invalid text encoding".into(),
                    }]
                } else {
                    Vec::new()
                },
                ..IndexResult::default()
            };
            let mut output = Vec::new();
            write_index_result(&mut output, &root, &result).expect("index output");
            let output = String::from_utf8(output).expect("UTF-8");
            assert!(output.starts_with("Workspace index\nfiles\t"));
            assert!(!output.contains("Workspace index: ready"));
            assert!(output.contains("1 retried"));
            assert!(output.contains(&format!("{failed} failed")));
            if failed > 0 {
                assert!(output.contains("failed\tbroken.txt: prepare: invalid text encoding"));
            }
        }
    }

    #[test]
    fn index_summary_uses_node_fields_and_reports_retries_and_duration() {
        let result = IndexResult {
            files_scanned: 9,
            files_added: 2,
            files_modified: 1,
            files_pending: 1,
            files_unchanged: 4,
            files_deleted: 3,
            files_failed: 1,
            entities_created: 24,
            duration_micros: 61_500_000,
            ..IndexResult::default()
        };
        let mut bytes = Vec::new();
        write_index_result(&mut bytes, Path::new("/workspace"), &result).expect("render output");
        assert_eq!(
            String::from_utf8(bytes).expect("render output"),
            "Workspace index\nfiles\t9 scanned, 2 added, 1 modified, 1 retried, 4 unchanged, 3 deleted, 1 failed\nentities\t24\nduration\t1m 2s (61500ms)\nroots\t/workspace\n",
        );
    }

    #[test]
    fn index_colors_distinguish_failures_and_respect_explicit_color_mode() {
        let result = IndexResult {
            files_scanned: 1,
            files_failed: 1,
            ..IndexResult::default()
        };
        for (color, terminal, styled) in [
            (ColorMode::Always, false, true),
            (ColorMode::Never, true, false),
            (ColorMode::Auto, false, false),
        ] {
            let mut bytes = Vec::new();
            write_index_with_options(
                &mut bytes,
                Path::new("/workspace"),
                &result,
                None,
                OutputOptions {
                    color,
                    ..OutputOptions::default()
                },
                terminal,
            )
            .expect("render output");
            let output = String::from_utf8(bytes).expect("render output");
            assert_eq!(output.contains("\x1b[31m1 failed\x1b[0m"), styled);
            assert_eq!(output.contains("\x1b["), styled);
        }
    }

    #[test]
    fn index_summary_displays_saved_scan_rules() {
        let scan = ScanRules {
            file_types: vec!["rust".into(), "python".into()],
            excluded_file_types: vec!["js".into()],
            globs: vec![
                "*.rs".into(),
                zg_engine::api::index::options::GlobRule {
                    pattern: "!vendor/**".into(),
                    case_insensitive: true,
                },
            ],
            hidden: true,
            no_ignore: true,
            ignore_files: vec!["custom.ignore".into()],
            max_depth: Some(2),
            max_file_size_bytes: Some(4096),
            follow_symlinks: true,
            nested_git: false,
        };
        let mut bytes = Vec::new();
        write_index_with_options(
            &mut bytes,
            Path::new("/workspace"),
            &IndexResult::default(),
            Some(&scan),
            OutputOptions::default(),
            false,
        )
        .expect("summary");
        let output = String::from_utf8(bytes).expect("UTF-8");
        assert!(output.contains("\nroots\t/workspace (glob=*.rs iglob=!vendor/** type=rust|python type-not=js hidden no-ignore ignore-file=custom.ignore max-depth=2 max-filesize=4096 follow nested-git=false)\n"));
    }

    #[test]
    fn empty_index_has_the_node_file_type_tip() {
        let mut bytes = Vec::new();
        write_index_result(&mut bytes, Path::new("/workspace"), &IndexResult::default())
            .expect("render output");
        let output = String::from_utf8(bytes).expect("render output");
        assert!(output.contains("duration\t0ms (0ms)"));
        assert!(output.contains("tip\tNo indexable files were found. Run `zg --help file-types`"));
    }

    #[test]
    fn authorization_groups_verified_destinations_and_relative_storage() {
        let status = AuthorizationStatus {
            root: "/workspace".into(),
            path: "/workspace/.zvec-grep/authorization.json".into(),
            grants: vec![AuthorizationGrantStatus {
                model: "qwen/text-embedding-v4".into(),
                endpoint: "https://provider.test:8443/v1/embeddings".into(),
                endpoint_host: "provider.test:8443".into(),
            }],
        };
        let mut bytes = Vec::new();
        write_authorization_status(&mut bytes, &status, ColorMode::Never, true)
            .expect("render output");
        let output = String::from_utf8(bytes).expect("render output");
        assert!(
            output.starts_with("✓ Remote Embedding is authorized\n  /workspace\n\nAuthorization\n")
        );
        assert!(output.contains("Workspace"));
        assert!(output.contains("qwen/text-embedding-v4"));
        assert!(output.contains("provider.test:8443"));
        assert!(!output.contains("https://"));
        assert!(output.contains("\nStorage\n"));
        assert!(output.contains(".zvec-grep/authorization.json"));
        assert!(!output.contains("\x1b["));
    }

    #[test]
    fn missing_authorization_shows_grant_command() {
        let status = AuthorizationStatus {
            root: "/workspace".into(),
            path: "/workspace/.zvec-grep/authorization.json".into(),
            grants: Vec::new(),
        };
        let mut bytes = Vec::new();
        write_authorization_status(&mut bytes, &status, ColorMode::Never, false)
            .expect("render output");
        assert_eq!(
            String::from_utf8(bytes).expect("render output"),
            "○ Remote Embedding is not authorized\n  /workspace\n\nRun\n  zg --auth grant --capability embedding --scope workspace\n",
        );
    }
}
