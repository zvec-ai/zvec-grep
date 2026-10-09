//! Workspace status in the same grouped layout as the Node.js CLI.

use std::io::{self, Write};

use zg_engine::{
    api::index::options::ScanRules,
    api::info::{
        InfoResult,
        result::{
            IndexCompatibility, IndexStats, IndexStatus, WorkspaceIndexInfo, WorkspaceIndexPolicy,
        },
    },
};

use crate::{
    ColorMode, OutputOptions,
    theme::{StatusTheme, display_path, format_count, storage_path, write_status_field},
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
    writeln!(writer, "{}", heading(theme, result.index_status()))?;
    writeln!(writer, "  {}", theme.path(&display_path(&result.root)))?;

    if let Some(stats) = &result.status {
        writeln!(writer)?;
        write_statistics(&mut writer, theme, stats)?;
    }
    if let Some(index) = &result.workspace_index {
        writeln!(writer)?;
        write_embedding(&mut writer, theme, index)?;
    }

    writeln!(writer)?;
    write_status_field(
        &mut writer,
        theme,
        "Storage",
        &[theme.path(&storage_path(&result.index_path, &result.root))],
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
                &[theme.warning(&format!("{actual} (expected {expected_version})"))],
            )?;
        }
    }
    if let Some(index) = &result.workspace_index {
        write_status_field(
            &mut writer,
            theme,
            "Nested Git",
            &[if index.scan.nested_git {
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
            write_status_field(&mut writer, theme, "Roots", &[theme.path(&root)])?;
        }
    }
    match result.index_policy {
        WorkspaceIndexPolicy::Enabled => {}
        WorkspaceIndexPolicy::Disabled => {
            write_status_field(&mut writer, theme, "Policy", &[theme.warning("disabled")])?;
        }
        WorkspaceIndexPolicy::Uninitialized => {
            write_status_field(&mut writer, theme, "Policy", &[theme.muted("undecided")])?;
        }
    }
    write_diagnostics(&mut writer, theme, result)
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

fn write_statistics(
    writer: &mut impl Write,
    theme: StatusTheme,
    stats: &IndexStats,
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
    let unicode = !cfg!(windows) && std::env::var("TERM").as_deref() != Ok("linux");
    let bar = crate::progress::gradient_bar(filled, 20, theme.color, unicode);
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
    if stats.files_added > 0 || stats.files_modified > 0 || stats.files_deleted > 0 {
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
) -> io::Result<()> {
    use zg_engine::api::context::options::ContentKind;
    let values = index.default_model_ref.as_ref().map_or_else(
        || vec![theme.warning("Not configured")],
        |reference| vec![reference.clone()],
    );
    write_status_field(writer, theme, "Default", &values)?;
    for model in &index.embeddings {
        write_status_field(
            writer,
            theme,
            "Model",
            &[format!(
                "{}/{} ({} dimensions, {})",
                model.provider,
                model.model,
                format_count(model.dimension),
                model.metric,
            )],
        )?;
    }
    for kind in [ContentKind::Text, ContentKind::Code, ContentKind::Image] {
        let reference = index
            .embedding_routes
            .get(&kind)
            .or(index.default_model_ref.as_ref());
        let route = reference.and_then(|reference| {
            index
                .embeddings
                .iter()
                .find(|model| {
                    format!("{}/{}", model.provider, model.model) == *reference
                        && model.content_kinds.contains(&kind)
                })
                .map(|_| reference)
        });
        let value = route.map_or_else(
            || "not indexed".to_owned(),
            |reference| {
                format!(
                    "{reference} ({})",
                    if index.embedding_routes.contains_key(&kind) {
                        "explicit"
                    } else {
                        "default"
                    }
                )
            },
        );
        write_status_field(writer, theme, kind.as_str(), &[value])?;
    }
    if let Some(fts) = &index.fts {
        write_status_field(
            writer,
            theme,
            "FTS",
            &[format!(
                "tokenizer={} filters={}",
                fts.tokenizer,
                fts.filters.join(", ")
            )],
        )?;
    }
    Ok(())
}

fn write_diagnostics(
    writer: &mut impl Write,
    theme: StatusTheme,
    result: &InfoResult,
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
        write_status_field(writer, theme, "Problem", &[theme.danger(reason)])?;
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
                format!("{}: {compact}", path.display())
            })
            .collect();
        if files.len() > 3 {
            messages.push(format!("and {} more", files.len() - 3));
        }
        write_status_field(
            writer,
            theme,
            "Problem",
            &[theme.danger(&messages.join("; "))],
        )?;
    }
    if let Some(suggestion) = suggestion {
        write_status_field(writer, theme, "Next", &[theme.accent(suggestion)])?;
    }
    Ok(())
}

pub(crate) fn scan_filters(scan: &ScanRules) -> String {
    let mut filters: Vec<_> = scan
        .globs
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
    if scan.hidden {
        filters.push("hidden".into());
    }
    if scan.no_ignore {
        filters.push("no-ignore".into());
    }
    if !scan.ignore_files.is_empty() {
        filters.push(format!(
            "ignore-file={}",
            scan.ignore_files
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("|")
        ));
    }
    if let Some(depth) = scan.max_depth {
        filters.push(format!("max-depth={depth}"));
    }
    if let Some(size) = scan.max_file_size_bytes {
        filters.push(format!("max-filesize={size}"));
    }
    if scan.follow_symlinks {
        filters.push("follow".into());
    }
    if !scan.nested_git {
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
            index_path: "/workspace/.zvec-grep/storage".into(),
            source: InfoSource::Index,
            workspace_index: Some(WorkspaceIndexInfo {
                name: "workspace".into(),
                path: "/workspace/.zvec-grep".into(),
                root: "/workspace".into(),
                scan: ScanRules::default(),
                policy: WorkspaceIndexPolicy::Enabled,
                default_model_ref: Some("qwen/text-embedding-v4".into()),
                embedding_routes: std::collections::BTreeMap::default(),
                embeddings: vec![WorkspaceIndexEmbedding {
                    content_kinds: vec![
                        zg_engine::api::context::options::ContentKind::Text,
                        zg_engine::api::context::options::ContentKind::Code,
                    ],
                    provider: "qwen".into(),
                    model: "text-embedding-v4".into(),
                    dimension: 1024,
                    metric: "cosine".into(),
                }],
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
                "✓ Workspace index is ready\n  /workspace\n\n  Coverage    {bar} 100%  1,132 / 1,132 files\n  Entities    22,037\n  Source size 4,177,103 bytes\n  Queue       0 pending · 0 failed\n\n  Default     qwen/text-embedding-v4\n  Model       qwen/text-embedding-v4 (1,024 dimensions, cosine)\n  text        qwen/text-embedding-v4 (default)\n  code        qwen/text-embedding-v4 (default)\n  image       not indexed\n  FTS         tokenizer=jieba filters=lowercase\n\n  Storage     .zvec-grep/storage\n  Version     2\n  Nested Git  included\n"
            )
        );
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
        index.default_model_ref = None;
        index.embeddings.clear();
        index.scan.globs = vec!["*.rs".into(), "!vendor/**".into()];
        index.scan.hidden = true;
        index.scan.max_depth = Some(0);
        let output = render(&info, ColorMode::Never);
        assert!(output.contains("Default     Not configured"));
        assert!(
            output
                .contains("Roots       /workspace (glob=*.rs glob=!vendor/** hidden max-depth=0)")
        );
    }
}
