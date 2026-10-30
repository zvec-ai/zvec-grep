//! Workspace status with compact terminal headings and stable redirected output.

use std::{
    borrow::Cow,
    io::{self, Write},
};

use zg_engine::{
    api::index::options::{FileFormat, ScanRules},
    api::info::{
        InfoResult,
        result::{
            IndexCompatibility, IndexStats, IndexStatus, WorkspaceIndexInfo, WorkspaceIndexPolicy,
        },
    },
};

use crate::{
    ColorMode, OutputOptions, output_path,
    theme::{
        StatusTheme, clean, display_path, format_count, storage_path, unicode_supported,
        write_status_field,
    },
};

/// Writes workspace status with the requested color policy.
/// # Errors
/// Returns the underlying writer error.
pub fn write_info_with_options(
    mut writer: impl Write,
    result: &InfoResult,
    options: OutputOptions,
    terminal: bool,
) -> io::Result<()> {
    let theme = StatusTheme::new(options.color, terminal);
    writeln!(
        writer,
        "{}",
        if terminal {
            terminal_heading(theme, result.index_status())
        } else {
            heading(theme, result.index_status())
        }
    )?;
    writeln!(
        writer,
        "  {}",
        theme.path(&human_text(&display_path(&result.root), terminal))
    )?;

    if let Some(stats) = &result.status {
        writeln!(writer)?;
        write_statistics(&mut writer, theme, stats, terminal)?;
    }
    if let Some(index) = &result.workspace_index {
        writeln!(writer)?;
        write_embedding(&mut writer, theme, index, terminal)?;
    }

    writeln!(writer)?;
    write_status_field(
        &mut writer,
        theme,
        "Storage",
        &[theme.path(&human_text(
            &storage_path(&result.index_path, &result.root),
            terminal,
        ))],
    )?;
    match &result.compatibility {
        IndexCompatibility::Unbuilt => {}
        IndexCompatibility::Compatible { version } => {
            write_status_field(&mut writer, theme, "Version", &[version.to_string()])?;
        }
        IndexCompatibility::RebuildRequired {
            actual_version,
            expected_version,
            ..
        } => {
            let actual =
                actual_version.map_or_else(|| "unknown".to_owned(), |version| version.to_string());
            write_status_field(
                &mut writer,
                theme,
                "Version",
                &[format!("{actual} (expected {expected_version})")],
            )?;
        }
    }
    if let Some(index) = &result.workspace_index {
        write_status_field(
            &mut writer,
            theme,
            "Nested Git",
            &[if index.scan.nested_git() {
                "included"
            } else {
                "excluded"
            }
            .into()],
        )?;
        let filters = scan_filters(&index.scan);
        if index.root != result.root || !filters.is_empty() {
            let root = display_path(&index.root);
            let root = if filters.is_empty() {
                root
            } else {
                format!("{root} ({filters})")
            };
            write_status_field(
                &mut writer,
                theme,
                "Roots",
                &[theme.path(&human_text(&root, terminal))],
            )?;
        }
    }
    match result.index_policy {
        WorkspaceIndexPolicy::Enabled => {}
        WorkspaceIndexPolicy::Disabled => {
            write_status_field(&mut writer, theme, "Policy", &["disabled".into()])?;
        }
        WorkspaceIndexPolicy::Uninitialized => {
            write_status_field(&mut writer, theme, "Policy", &[theme.muted("undecided")])?;
        }
    }
    write_diagnostics(&mut writer, theme, result, terminal)
}

/// Writes workspace index status without ANSI colors.
/// # Errors
/// Returns the underlying writer error.
pub fn write_info_result(writer: impl Write, result: &InfoResult) -> io::Result<()> {
    write_info_with_options(
        writer,
        result,
        OutputOptions {
            color: ColorMode::Never,
            ..OutputOptions::default()
        },
        false,
    )
}

fn heading(theme: StatusTheme, state: IndexStatus) -> String {
    match state {
        IndexStatus::Ready => theme.success("✓ Workspace index is ready"),
        IndexStatus::Stale => theme.warning("! Workspace index needs an update"),
        IndexStatus::Failed => theme.danger("✗ Workspace index failed"),
        IndexStatus::Disabled => theme.warning("○ Workspace indexing is disabled"),
        IndexStatus::Missing => theme.warning("○ Workspace index is not created"),
        IndexStatus::Uninitialized => theme.warning("? Workspace index is not configured"),
        IndexStatus::Unknown => theme.warning("? Workspace index status is unknown"),
        IndexStatus::RebuildRequired => theme.warning("! Workspace index requires a rebuild"),
    }
}

fn terminal_heading(theme: StatusTheme, state: IndexStatus) -> String {
    let unicode = unicode_supported();
    let (marker, label) = match state {
        IndexStatus::Ready => (
            theme.success(if unicode { "✓" } else { "+" }),
            "Index ready",
        ),
        IndexStatus::Stale => (theme.warning("!"), "Index needs an update"),
        IndexStatus::Failed => (
            theme.danger(if unicode { "×" } else { "x" }),
            "Index has failed files",
        ),
        IndexStatus::Disabled => (theme.warning("!"), "Indexing disabled"),
        IndexStatus::Missing => (theme.warning("!"), "Index not created"),
        IndexStatus::Uninitialized => (theme.warning("!"), "Index not configured"),
        IndexStatus::Unknown => (theme.warning("!"), "Index status unknown"),
        IndexStatus::RebuildRequired => (theme.warning("!"), "Index requires a rebuild"),
    };
    format!("{marker} {label}")
}

fn human_text(value: &str, terminal: bool) -> Cow<'_, str> {
    if terminal {
        Cow::Owned(clean(value))
    } else {
        Cow::Borrowed(value)
    }
}

fn write_statistics(
    writer: &mut impl Write,
    theme: StatusTheme,
    stats: &IndexStats,
    terminal: bool,
) -> io::Result<()> {
    // Stored/indexed counts include changed and deleted snapshots. Coverage is
    // the current scan's unchanged files, matching Node's indexCompletionFromStatus.
    let completed = stats.files_unchanged;
    let total = stats.files_scanned;
    let ratio = |scale: u128| -> u128 {
        if total == 0 {
            0
        } else {
            let rounded =
                (completed.min(total) as u128 * scale + total as u128 / 2) / total as u128;
            // Rounding must not make an unfinished scan look complete.
            if completed == total {
                rounded
            } else {
                rounded.min(scale - 1)
            }
        }
    };
    let percent = ratio(100);
    let filled = usize::try_from(ratio(20)).unwrap_or(20);
    let bar = if terminal {
        crate::progress::block_bar(filled, 20, theme.color, crate::theme::unicode_supported())
    } else {
        let unicode = !cfg!(windows) && std::env::var("TERM").as_deref() != Ok("linux");
        crate::progress::gradient_bar(filled, 20, theme.color, unicode)
    };
    write_status_field(
        writer,
        theme,
        "Coverage",
        &[format!(
            "{bar} {percent:>3}%  {} / {} files",
            format_count(completed),
            format_count(total)
        )],
    )?;
    write_status_field(
        writer,
        theme,
        "Entities",
        &[format_count(stats.entities_indexed)],
    )?;
    // Rust indexes whole source snapshots and has no fragment truncation counter.
    write_status_field(
        writer,
        theme,
        "Source size",
        &[format!("{} bytes", format_count(stats.indexed_size_bytes))],
    )?;
    write_queue_and_changes(writer, theme, stats, terminal)
}

fn write_queue_and_changes(
    writer: &mut impl Write,
    theme: StatusTheme,
    stats: &IndexStats,
    terminal: bool,
) -> io::Result<()> {
    if terminal {
        let queue = [
            (stats.files_pending, "pending"),
            (stats.files_failed, "failed"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, label)| format!("{} {label}", format_count(count)))
        .collect::<Vec<_>>();
        if !queue.is_empty() {
            write_status_field(writer, theme, "Queue", &[queue.join(" · ")])?;
        }
    } else {
        let pending = format!("{} pending", format_count(stats.files_pending));
        let failed = format!("{} failed", format_count(stats.files_failed));
        write_status_field(
            writer,
            theme,
            "Queue",
            &[format!(
                "{} {} {}",
                if stats.files_pending > 0 {
                    theme.warning(&pending)
                } else {
                    theme.muted(&pending)
                },
                theme.muted("·"),
                if stats.files_failed > 0 {
                    theme.danger(&failed)
                } else {
                    theme.muted(&failed)
                }
            )],
        )?;
    }
    if stats.files_added > 0 || stats.files_modified > 0 || stats.files_deleted > 0 {
        if terminal {
            let changes = [
                (stats.files_added, "added"),
                (stats.files_modified, "modified"),
                (stats.files_deleted, "deleted"),
            ]
            .into_iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, label)| format!("{} {label}", format_count(count)))
            .collect::<Vec<_>>();
            return write_status_field(writer, theme, "Changes", &[changes.join(" · ")]);
        }
        write_status_field(
            writer,
            theme,
            "Changes",
            &[format!(
                "{} {} {} {} {}",
                theme.warning(&format!("{} added", format_count(stats.files_added))),
                theme.muted("·"),
                theme.warning(&format!("{} modified", format_count(stats.files_modified))),
                theme.muted("·"),
                theme.warning(&format!("{} deleted", format_count(stats.files_deleted)))
            )],
        )?;
    }
    Ok(())
}

fn write_embedding(
    writer: &mut impl Write,
    theme: StatusTheme,
    index: &WorkspaceIndexInfo,
    terminal: bool,
) -> io::Result<()> {
    let values = index.embedding.as_ref().map_or_else(
        || vec!["Not configured".into()],
        |embedding| {
            vec![
                format!(
                    "{}/{}",
                    human_text(&embedding.provider, terminal),
                    human_text(&embedding.model, terminal)
                ),
                format!(
                    "{} dimensions {} {}",
                    format_count(embedding.dimension),
                    theme.muted("·"),
                    human_text(&embedding.metric, terminal)
                ),
            ]
        },
    );
    write_status_field(writer, theme, "Embedding", &values)?;
    if let Some(fts) = &index.fts {
        write_status_field(
            writer,
            theme,
            "FTS",
            &[format!(
                "tokenizer={} filters={}",
                human_text(&fts.tokenizer, terminal),
                human_text(&fts.filters.join(", "), terminal)
            )],
        )?;
    }
    Ok(())
}

fn write_diagnostics(
    writer: &mut impl Write,
    theme: StatusTheme,
    result: &InfoResult,
    terminal: bool,
) -> io::Result<()> {
    let reason = match &result.compatibility {
        IndexCompatibility::RebuildRequired { reason, .. } => Some(reason.as_str()),
        _ => None,
    };
    let failed = result
        .status
        .as_ref()
        .map(|stats| &stats.failed_files)
        .filter(|files| !files.is_empty());
    let suggestion = match result.index_status() {
        IndexStatus::RebuildRequired => Some("zg --index --rebuild"),
        IndexStatus::Stale | IndexStatus::Failed => Some("zg --index"),
        _ => result.suggestion.as_deref(),
    };
    if reason.is_some() || failed.is_some() || suggestion.is_some() {
        writeln!(writer)?;
    }
    if let Some(reason) = reason {
        write_status_field(
            writer,
            theme,
            "Problem",
            &[if terminal {
                clean(reason)
            } else {
                theme.danger(reason)
            }],
        )?;
    }
    if let Some(files) = failed {
        let mut messages: Vec<_> = files
            .iter()
            .take(3)
            .map(|file| {
                let path = file.path.strip_prefix(&result.root).unwrap_or(&file.path);
                let compact = file.reason.split_whitespace().collect::<Vec<_>>().join(" ");
                let compact = if compact.chars().count() > 240 {
                    format!("{}...", compact.chars().take(237).collect::<String>())
                } else {
                    compact
                };
                human_text(
                    &format!("{}: {compact}", output_path(path).display()),
                    terminal,
                )
                .into_owned()
            })
            .collect();
        if files.len() > 3 {
            messages.push(format!("and {} more", files.len() - 3));
        }
        let messages = if terminal {
            messages
        } else {
            vec![theme.danger(&messages.join("; "))]
        };
        write_status_field(writer, theme, "Problem", &messages)?;
    }
    if let Some(suggestion) = suggestion {
        write_status_field(
            writer,
            theme,
            "Next",
            &[StatusTheme::accent(&human_text(suggestion, terminal))],
        )?;
    }
    Ok(())
}

pub(crate) fn scan_filters(scan: &ScanRules) -> String {
    let mut filters: Vec<_> = scan
        .globs()
        .iter()
        .map(|glob| {
            format!(
                "{}={}",
                if glob.case_insensitive {
                    "iglob"
                } else {
                    "glob"
                },
                glob.pattern
            )
        })
        .collect();
    for (name, formats) in [
        ("type", scan.included_formats()),
        ("type-not", scan.excluded_formats()),
    ] {
        if !formats.is_empty() {
            filters.push(format!(
                "{name}={}",
                formats
                    .iter()
                    .copied()
                    .map(FileFormat::as_str)
                    .collect::<Vec<_>>()
                    .join("|")
            ));
        }
    }
    if scan.include_hidden() {
        filters.push("hidden".into());
    }
    if !scan.use_default_ignores() {
        filters.push("no-ignore".into());
    }
    let ignore_files = scan
        .ignore_files()
        .map(|path| output_path(path).display().to_string())
        .collect::<Vec<_>>();
    if !ignore_files.is_empty() {
        filters.push(format!("ignore-file={}", ignore_files.join("|")));
    }
    if let Some(depth) = scan.max_depth() {
        filters.push(format!("max-depth={depth}"));
    }
    if let Some(size) = scan.max_file_size_bytes() {
        filters.push(format!("max-filesize={size}"));
    }
    if scan.follow_symlinks() {
        filters.push("follow".into());
    }
    if !scan.nested_git() {
        filters.push("nested-git=false".into());
    }
    filters.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zg_engine::api::info::result::{
        FailedFile, InfoSource, WorkspaceIndexEmbedding, WorkspaceIndexFts,
    };

    fn ready() -> InfoResult {
        InfoResult {
            root: "/workspace".into(),
            indexed: true,
            compatibility: IndexCompatibility::Compatible { version: 2 },
            index_policy: WorkspaceIndexPolicy::Enabled,
            home: "/workspace/.zvec-grep".into(),
            index_path: "/workspace/.zvec-grep/index".into(),
            source: InfoSource::Index,
            workspace_index: Some(WorkspaceIndexInfo {
                path: "/workspace/.zvec-grep".into(),
                root: "/workspace".into(),
                scan: ScanRules::default(),
                policy: WorkspaceIndexPolicy::Enabled,
                embedding: Some(WorkspaceIndexEmbedding {
                    provider: "qwen".into(),
                    model: "text-embedding-v4".into(),
                    dimension: 1024,
                    metric: "cosine".into(),
                }),
                fts: Some(WorkspaceIndexFts {
                    tokenizer: "jieba".into(),
                    filters: vec!["lowercase".into()],
                }),
                index_version: Some(2),
                created_epoch_ms: 0,
                updated_epoch_ms: 0,
            }),
            status: Some(IndexStats {
                files_scanned: 1132,
                files_stored: 1132,
                files_indexed: 1132,
                files_unchanged: 1132,
                entities_indexed: 22037,
                indexed_size_bytes: 4_177_103,
                ..IndexStats::default()
            }),
            suggestion: None,
        }
    }

    fn render(info: &InfoResult, color: ColorMode) -> String {
        let mut bytes = Vec::new();
        write_info_with_options(
            &mut bytes,
            info,
            OutputOptions {
                color,
                ..OutputOptions::default()
            },
            false,
        )
        .expect("status output");
        String::from_utf8(bytes).expect("UTF-8")
    }

    #[test]
    fn ready_status_matches_node_layout_with_rust_metadata() {
        let bar = crate::progress::gradient_bar(
            20,
            20,
            false,
            !cfg!(windows) && std::env::var("TERM").as_deref() != Ok("linux"),
        );
        assert_eq!(
            render(&ready(), ColorMode::Never),
            format!(
                "✓ Workspace index is ready\n  /workspace\n\n  Coverage    {bar} 100%  1,132 / 1,132 files\n  Entities    22,037\n  Source size 4,177,103 bytes\n  Queue       0 pending · 0 failed\n\n  Embedding   qwen/text-embedding-v4\n              1,024 dimensions · cosine\n  FTS         tokenizer=jieba filters=lowercase\n\n  Storage     .zvec-grep/index\n  Version     2\n  Nested Git  included\n"
            )
        );
    }

    #[test]
    fn terminal_status_has_compact_heading_and_retains_nonzero_attention_counts() {
        let mut info = ready();
        let mut bytes = Vec::new();
        let options = OutputOptions {
            color: ColorMode::Never,
            ..OutputOptions::default()
        };
        write_info_with_options(&mut bytes, &info, options, true).expect("ready output");
        let output = String::from_utf8(bytes).expect("UTF-8");
        let ready_marker = if unicode_supported() { "✓" } else { "+" };
        assert!(output.starts_with(&format!("{ready_marker} Index ready\n  /workspace\n")));
        assert!(output.contains("100%  1,132 / 1,132 files"));
        assert!(!output.contains("Queue"));

        let stats = info.status.as_mut().expect("stats");
        stats.files_unchanged -= 1;
        stats.files_modified = 1;
        stats.files_failed = 1;
        stats.failed_files = vec![FailedFile {
            path: "/workspace/failure.rs".into(),
            reason: "cannot read file".into(),
        }];
        let mut bytes = Vec::new();
        write_info_with_options(&mut bytes, &info, options, true).expect("failed output");
        let output = String::from_utf8(bytes).expect("UTF-8");
        let failed_marker = if unicode_supported() { "×" } else { "x" };
        assert!(output.starts_with(&format!("{failed_marker} Index has failed files\n")));
        assert!(output.contains("Queue       1 failed"));
        assert!(output.contains("Changes     1 modified"));
        assert!(output.contains("failure.rs: cannot read file"));
        assert!(output.contains("Next        zg --index"));
        assert!(!output.contains("0 added"));
        assert!(!output.contains("0 pending"));
    }

    #[test]
    fn terminal_status_sanitizes_dynamic_fields_without_rewriting_redirected_output() {
        let mut info = ready();
        info.root = "/workspace/\x1b[2J\nroot".into();
        info.index_path = "/storage/\x1b[2J\nindex".into();
        let index = info.workspace_index.as_mut().expect("index");
        let embedding = index.embedding.as_mut().expect("embedding");
        embedding.model = "model\x1b[2J\nname".into();
        let stats = info.status.as_mut().expect("stats");
        stats.files_failed = 1;
        stats.failed_files = vec![FailedFile {
            path: "/workspace/file\x1b[2J\nname.rs".into(),
            reason: "failed\x1b[2J\nreason".into(),
        }];
        let mut bytes = Vec::new();
        write_info_with_options(
            &mut bytes,
            &info,
            OutputOptions {
                color: ColorMode::Never,
                ..OutputOptions::default()
            },
            true,
        )
        .expect("terminal status");
        let terminal = String::from_utf8(bytes).expect("UTF-8");
        assert!(!terminal.contains('\x1b'));
        assert!(terminal.contains("model [2J name"));
        assert!(terminal.contains("file [2J name.rs: failed [2J reason"));
        assert!(render(&info, ColorMode::Never).contains("\x1b[2J\nroot"));
    }

    #[test]
    fn coverage_excludes_modified_and_deleted_snapshots() {
        let mut info = ready();
        info.status = Some(IndexStats {
            files_scanned: 173,
            files_stored: 193,
            files_indexed: 193,
            files_unchanged: 172,
            files_modified: 1,
            files_deleted: 20,
            ..IndexStats::default()
        });
        let output = render(&info, ColorMode::Never);
        assert!(output.starts_with("! Workspace index needs an update\n"));
        assert!(output.contains(" 99%  172 / 173 files"));
        assert!(output.contains("Changes     0 added · 1 modified · 20 deleted"));
        assert!(output.contains("Next        zg --index"));
        assert!(!output.contains("100%"));
    }

    #[test]
    fn incomplete_coverage_never_rounds_to_a_full_percentage_or_bar() {
        let unicode = !cfg!(windows) && std::env::var("TERM").as_deref() != Ok("linux");
        for (completed, total, percent, filled) in [
            (199, 200, 99, 19),
            (999, 1000, 99, 19),
            (200, 200, 100, 20),
            (0, 200, 0, 0),
            (0, 0, 0, 0),
        ] {
            let mut info = ready();
            info.status = Some(IndexStats {
                files_scanned: total,
                files_unchanged: completed,
                files_modified: total - completed,
                ..IndexStats::default()
            });
            for color in [ColorMode::Never, ColorMode::Always] {
                let bar =
                    crate::progress::gradient_bar(filled, 20, color == ColorMode::Always, unicode);
                let output = render(&info, color);
                assert!(
                    output.contains(&format!(
                        "{bar} {percent:>3}%  {} / {} files",
                        format_count(completed),
                        format_count(total)
                    )),
                    "{output}"
                );
                if completed < total {
                    assert!(output.contains("! Workspace index needs an update"));
                }
            }
        }
    }

    #[test]
    fn failures_are_red_pending_is_yellow_and_reasons_are_bounded() {
        let mut info = ready();
        info.status = Some(IndexStats {
            files_scanned: 4,
            files_unchanged: 2,
            files_pending: 1,
            files_failed: 1,
            failed_files: (0..4)
                .map(|index| FailedFile {
                    path: format!("/workspace/file{index}.rs").into(),
                    reason: format!("failed\n{}", "界".repeat(260)),
                })
                .collect(),
            ..IndexStats::default()
        });
        let output = render(&info, ColorMode::Always);
        assert!(output.starts_with("\x1b[31m✗ Workspace index failed\x1b[0m\n"));
        assert!(output.contains("\x1b[33m1 pending\x1b[0m"));
        assert!(output.contains("\x1b[31m1 failed\x1b[0m"));
        assert!(output.contains("\x1b[38;2;22;163;74m"));
        assert!(output.contains("file0.rs: failed "));
        assert!(output.contains("...; and 1 more"));
        assert!(!output.contains("file3.rs:"));
        assert!(!output.contains("failed\n界"));
    }

    #[test]
    fn unbuilt_disabled_and_unknown_do_not_claim_readiness() {
        let mut info = ready();
        info.status = None;
        assert!(
            render(&info, ColorMode::Never).starts_with("? Workspace index status is unknown\n")
        );
        info.indexed = false;
        assert!(render(&info, ColorMode::Never).starts_with("○ Workspace index is not created\n"));
        info.index_policy = WorkspaceIndexPolicy::Disabled;
        assert!(render(&info, ColorMode::Never).starts_with("○ Workspace indexing is disabled\n"));
        info.index_policy = WorkspaceIndexPolicy::Uninitialized;
        let output = render(&info, ColorMode::Never);
        assert!(output.starts_with("? Workspace index is not configured\n"));
        assert!(output.contains("Policy      undecided"));
        assert!(!output.contains("Coverage"));
    }

    #[test]
    fn incompatible_status_retains_versions_reason_and_rebuild_guidance() {
        for (actual_version, actual_label) in [(Some(1), "1"), (None, "unknown")] {
            let mut info = ready();
            info.index_policy = WorkspaceIndexPolicy::Disabled;
            info.compatibility = IndexCompatibility::RebuildRequired {
                actual_version,
                expected_version: 2,
                reason: "unsupported persisted index".into(),
            };
            info.suggestion = Some("zg --index".into());
            let output = render(&info, ColorMode::Never);
            assert!(output.starts_with("! Workspace index requires a rebuild\n"));
            assert!(output.contains(&format!("Version     {actual_label} (expected 2)")));
            assert!(output.contains("Problem     unsupported persisted index"));
            assert!(output.contains("Next        zg --index --rebuild"));
        }
    }

    #[test]
    fn scan_scope_and_missing_embedding_remain_visible() {
        let mut info = ready();
        let index = info.workspace_index.as_mut().expect("workspace");
        index.embedding = None;
        index.scan = zg_engine::api::index::options::ScanRulesUpdate {
            included_formats: Some(vec![FileFormat::Rust, FileFormat::TypeScript]),
            excluded_formats: Some(vec![FileFormat::Json]),
            globs: Some(vec!["*.rs".into(), "!vendor/**".into()]),
            include_hidden: Some(true),
            max_depth: Some(Some(0)),
            ..Default::default()
        }
        .apply(&index.scan)
        .expect("updated scan rules");
        let output = render(&info, ColorMode::Never);
        assert!(output.contains("Embedding   Not configured"));
        assert!(output.contains("Roots       /workspace (glob=*.rs glob=!vendor/** type=rust|typescript type-not=json hidden max-depth=0)"));
    }
}
