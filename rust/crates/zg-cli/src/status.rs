//! Compact workspace status: coverage, embedding, full-text search, and storage.

use std::{
    borrow::Cow,
    io::{self, Write},
};

use zg_engine::{
    api::index::options::{FileFormat, ScanRules},
    api::info::{
        InfoResult,
        result::{IndexStats, IndexStatus, WorkspaceIndexInfo},
    },
};

use crate::{
    ColorMode, OutputOptions, output_path,
    theme::{
        StatusTheme, clean, format_count, storage_path, unicode_supported, write_status_field,
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
    write_coverage(
        &mut writer,
        theme,
        result.status.as_ref(),
        result.index_status(),
    )?;
    writeln!(writer)?;
    write_embedding(
        &mut writer,
        theme,
        result.workspace_index.as_ref(),
        terminal,
    )?;
    writeln!(writer)?;
    write_status_field(
        &mut writer,
        theme,
        "Storage",
        &[theme.path(&human_text(
            &storage_path(&result.index_path, &result.root),
            terminal,
        ))],
    )
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

fn human_text(value: &str, terminal: bool) -> Cow<'_, str> {
    if terminal {
        Cow::Owned(clean(value))
    } else {
        Cow::Borrowed(value)
    }
}

fn write_coverage(
    writer: &mut impl Write,
    theme: StatusTheme,
    stats: Option<&IndexStats>,
    readiness: IndexStatus,
) -> io::Result<()> {
    let mut value = if let Some(stats) = stats {
        // Coverage counts unchanged files in the current scan, not stored
        // snapshots that may have been modified or deleted since indexing.
        let completed = stats.files_unchanged;
        let total = stats.files_scanned;
        let ratio = |scale: u128| -> u128 {
            if total == 0 {
                0
            } else {
                let rounded =
                    (completed.min(total) as u128 * scale + total as u128 / 2) / total as u128;
                if completed == total {
                    rounded
                } else {
                    rounded.min(scale - 1)
                }
            }
        };
        let percent = ratio(100);
        let filled = usize::try_from(ratio(20)).unwrap_or(20);
        let bar = crate::progress::gradient_bar(filled, 20, theme.color, unicode_supported());
        format!(
            "{bar} {percent:>3}%  {} / {} files",
            format_count(completed),
            format_count(total)
        )
    } else {
        "Unavailable".into()
    };
    let note = match readiness {
        IndexStatus::Ready => None,
        IndexStatus::Stale => Some("needs update"),
        IndexStatus::Failed => Some("failed files"),
        IndexStatus::Disabled => Some("indexing disabled"),
        IndexStatus::Missing => Some("index not created"),
        IndexStatus::Uninitialized => Some("index not configured"),
        IndexStatus::Unknown => Some("status unknown"),
        IndexStatus::RebuildRequired => Some("rebuild required"),
    };
    if let Some(note) = note {
        let note = if readiness == IndexStatus::Failed {
            theme.danger(note)
        } else {
            theme.warning(note)
        };
        value.push_str(" (");
        value.push_str(&note);
        value.push(')');
    }
    write_status_field(writer, theme, "Coverage", &[value])
}

fn write_embedding(
    writer: &mut impl Write,
    theme: StatusTheme,
    index: Option<&WorkspaceIndexInfo>,
    terminal: bool,
) -> io::Result<()> {
    let values = index
        .and_then(|index| index.embedding.as_ref())
        .map_or_else(
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
    let fts = index.and_then(|index| index.fts.as_ref()).map_or_else(
        || "Not configured".into(),
        |fts| {
            format!(
                "tokenizer={} filters={}",
                human_text(&fts.tokenizer, terminal),
                human_text(&fts.filters.join(", "), terminal)
            )
        },
    );
    write_status_field(writer, theme, "FTS", &[fts])
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
        FailedFile, IndexCompatibility, InfoSource, WorkspaceIndexEmbedding, WorkspaceIndexFts,
        WorkspaceIndexPolicy,
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
    fn ready_status_contains_only_the_four_requested_fields() {
        let bar = crate::progress::gradient_bar(20, 20, false, unicode_supported());
        let expected = format!(
            "  Coverage    {bar} 100%  1,132 / 1,132 files\n\n  Embedding   qwen/text-embedding-v4\n              1,024 dimensions · cosine\n  FTS         tokenizer=jieba filters=lowercase\n\n  Storage     .zvec-grep/index\n"
        );
        assert_eq!(render(&ready(), ColorMode::Never), expected);
        let mut bytes = Vec::new();
        write_info_with_options(
            &mut bytes,
            &ready(),
            OutputOptions {
                color: ColorMode::Never,
                ..OutputOptions::default()
            },
            true,
        )
        .expect("terminal output");
        assert_eq!(String::from_utf8(bytes).expect("UTF-8"), expected);
    }

    #[test]
    fn terminal_status_keeps_failure_state_inside_coverage() {
        let mut info = ready();
        let stats = info.status.as_mut().expect("stats");
        stats.files_unchanged -= 1;
        stats.files_modified = 1;
        stats.files_failed = 1;
        stats.failed_files = vec![FailedFile {
            path: "/workspace/failure.rs".into(),
            reason: "cannot read file".into(),
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
        .expect("failed output");
        let output = String::from_utf8(bytes).expect("UTF-8");
        assert!(output.contains("99%  1,131 / 1,132 files (failed files)"));
        for removed in ["Queue", "Changes", "failure.rs", "Next", "Index ready"] {
            assert!(!output.contains(removed), "{output}");
        }
    }

    #[test]
    fn terminal_status_sanitizes_dynamic_fields_without_rewriting_redirected_output() {
        let mut info = ready();
        info.index_path = "/storage/\x1b[2J\nindex".into();
        let embedding = info
            .workspace_index
            .as_mut()
            .expect("index")
            .embedding
            .as_mut()
            .expect("embedding");
        embedding.model = "model\x1b[2J\nname".into();
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
        assert!(terminal.contains("/storage/ [2J index"));
        assert!(render(&info, ColorMode::Never).contains("\x1b[2J\nindex"));
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
            indexed_size_bytes: 1_747_503,
            ..IndexStats::default()
        });
        let output = render(&info, ColorMode::Never);
        assert!(output.contains("99%  172 / 173 files (needs update)"));
        assert!(!output.contains("100%"));
        assert!(!output.contains("Source size"));
        assert!(!output.contains("Changes"));
    }

    #[test]
    fn incomplete_coverage_never_rounds_to_a_full_percentage_or_bar() {
        let unicode = unicode_supported();
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
                    assert!(output.contains("needs update"));
                }
            }
        }
    }

    #[test]
    fn failed_status_uses_red_without_expanding_the_field_list() {
        let mut info = ready();
        info.status = Some(IndexStats {
            files_scanned: 4,
            files_unchanged: 2,
            files_pending: 1,
            files_failed: 1,
            ..IndexStats::default()
        });
        let output = render(&info, ColorMode::Always);
        assert!(output.contains("\x1b[31mfailed files\x1b[0m"));
        assert!(output.contains("\x1b[38;2;22;163;74m"));
        assert!(!output.contains("Queue"));
        assert!(!output.contains("Problem"));
    }

    #[test]
    fn unbuilt_disabled_and_unknown_do_not_claim_readiness() {
        let mut info = ready();
        info.status = None;
        assert!(render(&info, ColorMode::Never).contains("Unavailable (status unknown)"));
        info.indexed = false;
        assert!(render(&info, ColorMode::Never).contains("Unavailable (index not created)"));
        info.index_policy = WorkspaceIndexPolicy::Disabled;
        assert!(render(&info, ColorMode::Never).contains("Unavailable (indexing disabled)"));
        info.index_policy = WorkspaceIndexPolicy::Uninitialized;
        info.workspace_index = None;
        let output = render(&info, ColorMode::Never);
        assert_eq!(
            output,
            "  Coverage    Unavailable (index not configured)\n\n  Embedding   Not configured\n  FTS         Not configured\n\n  Storage     .zvec-grep/index\n"
        );
    }

    #[test]
    fn incompatible_status_keeps_rebuild_state_inside_coverage() {
        for actual_version in [Some(1), None] {
            let mut info = ready();
            info.index_policy = WorkspaceIndexPolicy::Disabled;
            info.compatibility = IndexCompatibility::RebuildRequired {
                actual_version,
                expected_version: 2,
                reason: "unsupported persisted index".into(),
            };
            info.suggestion = Some("zg --index".into());
            let output = render(&info, ColorMode::Never);
            assert!(output.contains("(rebuild required)"));
            for removed in ["Version", "Problem", "Next", "Policy"] {
                assert!(!output.contains(removed), "{output}");
            }
        }
    }

    #[test]
    fn missing_configuration_is_explicit_and_scan_scope_is_hidden() {
        let mut info = ready();
        let index = info.workspace_index.as_mut().expect("workspace");
        index.embedding = None;
        index.fts = None;
        index.scan = zg_engine::api::index::options::ScanRulesUpdate {
            included_formats: Some(vec![FileFormat::Rust, FileFormat::TypeScript]),
            max_depth: Some(Some(0)),
            ..Default::default()
        }
        .apply(&index.scan)
        .expect("updated scan rules");
        let output = render(&info, ColorMode::Never);
        assert!(output.contains("Embedding   Not configured"));
        assert!(output.contains("FTS         Not configured"));
        assert!(!output.contains("Roots"));
        assert!(!output.contains("max-depth"));
    }
}
