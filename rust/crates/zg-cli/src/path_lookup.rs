//! Presentation of current file names and literal source matches.

use std::{
    io::{self, Write},
    path::Path,
};

#[cfg(windows)]
use std::path::{Component, Prefix};

use grep::{matcher::Matcher, regex::RegexMatcherBuilder};
use zg_engine::api::{
    context::{
        ContextResult,
        options::SymbolType,
        result::{
            ContentRange, ContextCoverage, ContextItem, ContextItemKind, ContextItemStatus,
            EmptyReason, EntityMetadata,
        },
    },
    paths::PathLookupResult,
};

use crate::{OutputOptions, theme::StatusTheme};

/// Writes current-file lookup output and fallback or incomplete-coverage warnings.
///
/// # Errors
/// Returns the underlying output or diagnostic writer error.
pub fn write_path_lookup_result(
    writer: impl Write,
    diagnostics: impl Write,
    result: &PathLookupResult,
    options: OutputOptions,
    terminal: bool,
) -> io::Result<()> {
    write_path_lookup_with_terminals(writer, diagnostics, result, options, terminal, terminal)
}

/// Writes lookup output with independent stdout and diagnostic terminal policies.
///
/// # Errors
/// Returns the underlying output or diagnostic writer error.
pub fn write_path_lookup_with_terminals(
    mut writer: impl Write,
    mut diagnostics: impl Write,
    result: &PathLookupResult,
    options: OutputOptions,
    stdout_terminal: bool,
    diagnostics_terminal: bool,
) -> io::Result<()> {
    match result {
        PathLookupResult::Files(result) => {
            for (index, path) in result.paths.iter().enumerate() {
                writeln!(writer, "#{} matchedBy=path {}", index + 1, path_label(path))?;
            }
            if result.truncated {
                write_warning(
                    &mut diagnostics,
                    "More matching files exist",
                    "Use --limit to show more",
                    "more matching files exist; use --limit to show more",
                    options,
                    diagnostics_terminal,
                )?;
            }
        }
        PathLookupResult::Text(result) => {
            write_warning(
                &mut diagnostics,
                "No matching file paths",
                "Searching current file contents literally (rg)",
                "no matching file paths; searching current file contents literally (rg).",
                options,
                diagnostics_terminal,
            )?;
            write_current_text(&mut writer, result, options, stdout_terminal)?;
            if result.coverage == ContextCoverage::RgTruncated {
                write_warning(
                    &mut diagnostics,
                    "Search coverage is incomplete",
                    "Result or scan limit reached; narrow the query or use --limit to show more",
                    "text search reached the result or scan limit; search coverage is incomplete; narrow the query or use --limit to show more",
                    options,
                    diagnostics_terminal,
                )?;
            }
        }
    }
    Ok(())
}

fn write_warning(
    writer: &mut impl Write,
    title: &str,
    detail: &str,
    plain: &str,
    options: OutputOptions,
    terminal: bool,
) -> io::Result<()> {
    if !terminal {
        return writeln!(writer, "warning: {plain}");
    }
    let theme = StatusTheme::new(options.color, terminal);
    writeln!(writer, "{} {title}\n  {detail}", theme.warning("!"))
}

fn write_current_text(
    writer: &mut impl Write,
    result: &ContextResult,
    options: OutputOptions,
    terminal: bool,
) -> io::Result<()> {
    let theme = StatusTheme::new(options.color, terminal);
    let groups: Vec<_> = result
        .items
        .chunk_by(|left, right| left.relative_path == right.relative_path)
        .collect();
    if options.preview == crate::PreviewMode::Detail {
        field(
            writer,
            theme,
            "Context",
            &format!("{} rg", path_label(&result.root)),
        )?;
        field(
            writer,
            theme,
            "Query",
            &highlight(&result.query, &result.query, theme.color),
        )?;
        let coverage = match result.coverage {
            ContextCoverage::RgExhaustive => "rg_exhaustive",
            ContextCoverage::RgTruncated => "rg_truncated",
            ContextCoverage::RankedSample => "ranked_sample",
        };
        field(writer, theme, "Coverage", coverage)?;
        field(writer, theme, "Files", &groups.len().to_string())?;
        field(writer, theme, "Hits", &result.items.len().to_string())?;
    }
    if groups.is_empty() {
        let reason = match result.diagnostics.empty_reason {
            Some(EmptyReason::NoSearchableFiles) => "No searchable files",
            Some(EmptyReason::NoMatches) | None => "No matches",
        };
        if options.preview == crate::PreviewMode::Detail {
            field(writer, theme, "Reason", reason)?;
        } else {
            writeln!(writer, "{reason}.")?;
        }
        return Ok(());
    }
    for group in groups {
        if options.preview == crate::PreviewMode::Detail {
            writeln!(writer)?;
            field(
                writer,
                theme,
                "File",
                &theme.path(&path_label(&group[0].relative_path)),
            )?;
            field(writer, theme, "Hits", &group.len().to_string())?;
        } else {
            writeln!(writer, "{}", path_label(&group[0].relative_path))?;
        }
        for item in group {
            if options.preview == crate::PreviewMode::Detail {
                write_detail_item(writer, item, theme)?;
                writeln!(writer, "  {}:", theme.label("Source"))?;
            } else if let Some(label) = structural_label(item) {
                let range = &item.container.as_ref().expect("structural container").range;
                writeln!(writer, "  {} [{label}]", range_label(range))?;
            }
            let indent = if options.preview == crate::PreviewMode::Detail
                || structural_label(item).is_some()
            {
                "    "
            } else {
                "  "
            };
            write_source(writer, item, &result.query, indent, theme.color)?;
        }
    }
    Ok(())
}

fn write_detail_item(
    writer: &mut impl Write,
    item: &ContextItem,
    theme: StatusTheme,
) -> io::Result<()> {
    let summary = match &item.metadata {
        Some(EntityMetadata::Code(code)) => {
            let kind = symbol_type_label(code.symbol_type);
            code.symbol_name
                .as_ref()
                .map_or_else(|| kind.to_owned(), |name| format!("{kind} {name}"))
        }
        Some(EntityMetadata::Markdown(markdown)) => markdown.heading.as_ref().map_or_else(
            || "markdown".to_owned(),
            |heading| format!("heading {heading}"),
        ),
        None => match item.kind {
            ContextItemKind::LexicalMatch => "lexical_match".to_owned(),
            ContextItemKind::IndexedEntity => "indexed_entity".to_owned(),
        },
    };
    writeln!(writer)?;
    writeln!(
        writer,
        "  {} {summary} matchedBy={}",
        StatusTheme::accent(&format!("#{}", item.rank)),
        StatusTheme::accent("lexical")
    )?;
    let status = match item.status {
        ContextItemStatus::Fresh => "fresh",
        ContextItemStatus::PossiblyStale => "possibly_stale",
    };
    writeln!(
        writer,
        "  {}: {}  {}: {status}",
        theme.label("Range"),
        range_label(&item.range),
        theme.label("Status")
    )?;
    match &item.metadata {
        Some(EntityMetadata::Code(code)) => {
            writeln!(
                writer,
                "  Kind: code/{}",
                symbol_type_label(code.symbol_type)
            )?;
            if let Some(name) = &code.symbol_name {
                writeln!(writer, "  Symbol: {name}")?;
            }
            if let Some(scope) = &code.scope {
                writeln!(writer, "  Scope: {scope}")?;
            }
            if let Some(signature) = &code.signature {
                writeln!(
                    writer,
                    "  Signature: {}",
                    signature.split_whitespace().collect::<Vec<_>>().join(" ")
                )?;
            }
        }
        Some(EntityMetadata::Markdown(markdown)) => {
            writeln!(writer, "  Kind: markdown")?;
            if let Some(heading) = &markdown.heading {
                writeln!(writer, "  Heading: {heading}")?;
            }
            if let Some(level) = markdown.level {
                writeln!(writer, "  Heading level: {level}")?;
            }
            if let Some(scope) = &markdown.scope {
                writeln!(writer, "  Scope: {scope}")?;
            }
        }
        None => {}
    }
    Ok(())
}

fn write_source(
    writer: &mut impl Write,
    item: &ContextItem,
    query: &str,
    indent: &str,
    color: bool,
) -> io::Result<()> {
    let matched = item.excerpt_range.as_ref().unwrap_or(&item.range);
    for (offset, line) in item.content.lines().enumerate() {
        let source = highlight(line, query, color);
        if let Some(first) = item.content_range.start_line() {
            let number = first + offset;
            let marker = if matched
                .start_line()
                .zip(matched.last_line())
                .is_none_or(|(first, last)| (first..=last).contains(&number))
            {
                ':'
            } else {
                '-'
            };
            writeln!(writer, "{indent}{number}{marker}\t{source}")?;
        } else {
            writeln!(writer, "{indent}{source}")?;
        }
    }
    Ok(())
}

fn structural_label(item: &ContextItem) -> Option<String> {
    let metadata = item
        .container
        .as_ref()?
        .metadata
        .as_ref()
        .or(item.metadata.as_ref())?;
    match metadata {
        EntityMetadata::Code(code) => {
            let name = code.symbol_name.as_ref()?;
            let qualified = code
                .scope
                .as_ref()
                .map_or_else(|| name.clone(), |scope| format!("{scope}.{name}"));
            Some(format!(
                "{} {qualified}",
                symbol_type_label(code.symbol_type)
            ))
        }
        EntityMetadata::Markdown(markdown) => {
            let heading = markdown.heading.as_ref()?;
            let qualified = markdown
                .scope
                .as_ref()
                .map_or_else(|| heading.clone(), |scope| format!("{scope} > {heading}"));
            Some(format!("heading {qualified}"))
        }
    }
}

fn symbol_type_label(kind: Option<SymbolType>) -> &'static str {
    match kind {
        Some(SymbolType::Alias) => "alias",
        Some(SymbolType::Class) => "class",
        Some(SymbolType::Enum) => "enum",
        Some(SymbolType::Function) => "function",
        Some(SymbolType::Interface) => "interface",
        Some(SymbolType::Module) => "module",
        Some(SymbolType::Value) => "value",
        None => "code",
    }
}

fn range_label(range: &ContentRange) -> String {
    match range {
        ContentRange::Text { start_line, .. } => {
            let last = range.last_line().expect("text range");
            if *start_line == last {
                start_line.to_string()
            } else {
                format!("{start_line}-{last}")
            }
        }
        ContentRange::Byte {
            start_offset,
            end_offset,
        } => format!("bytes:{start_offset}-{end_offset}"),
        ContentRange::File => "file".to_owned(),
    }
}

fn path_label(path: &Path) -> String {
    let path = crate::output_path(path);
    #[cfg(windows)]
    if matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(
                prefix.kind(),
                Prefix::Verbatim(_) | Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(..) | Prefix::DeviceNS(_)
            )
    ) {
        return path.display().to_string();
    }
    let path = path.to_string_lossy().replace('\\', "/");
    path.strip_prefix("//?/UNC/").map_or_else(
        || path.strip_prefix("//?/").unwrap_or(&path).to_owned(),
        |tail| format!("//{tail}"),
    )
}

fn field(writer: &mut impl Write, theme: StatusTheme, label: &str, value: &str) -> io::Result<()> {
    writeln!(writer, "{}: {value}", theme.label(label))
}

fn highlight(value: &str, query: &str, color: bool) -> String {
    if !color || query.is_empty() {
        return value.to_owned();
    }
    let Ok(matcher) = RegexMatcherBuilder::new()
        .fixed_strings(true)
        .case_insensitive(query == query.to_lowercase())
        .build(query)
    else {
        return value.to_owned();
    };
    let mut output = String::new();
    let mut cursor = 0;
    if matcher
        .find_iter(value.as_bytes(), |found| {
            output.push_str(&value[cursor..found.start()]);
            output.push_str("\x1b[1;33m");
            output.push_str(&value[found.start()..found.end()]);
            output.push_str("\x1b[0m");
            cursor = found.end();
            true
        })
        .is_err()
    {
        return value.to_owned();
    }
    output.push_str(&value[cursor..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColorMode;
    use zg_engine::api::{
        context::result::{ContextContentRole, ContextDiagnostics, ContextSource, MatchedBy},
        paths::PathsResult,
    };

    fn text_result() -> ContextResult {
        let root = std::env::temp_dir();
        let content = "before\nImport src/auth.ts\nafter\n";
        let range = ContentRange::Text {
            start_line: 1,
            end_line: 4,
            start_byte_offset: 0,
            end_byte_offset: content.len(),
            start_byte_column: 0,
            end_byte_column: 0,
        };
        ContextResult {
            query: "src/auth.ts".into(),
            freshness: None,
            background_refresh: None,
            indexing: None,
            root: root.clone(),
            source: ContextSource::Rg,
            coverage: ContextCoverage::RgExhaustive,
            workspace_index: None,
            items: vec![ContextItem {
                kind: ContextItemKind::LexicalMatch,
                rank: 1,
                absolute_path: root.join("refs.txt"),
                relative_path: "refs.txt".into(),
                range: range.clone(),
                excerpt_range: Some(ContentRange::Text {
                    start_line: 2,
                    end_line: 2,
                    start_byte_offset: 14,
                    end_byte_offset: 25,
                    start_byte_column: 7,
                    end_byte_column: 18,
                }),
                content_range: range,
                match_ranges: Vec::new(),
                content: content.into(),
                outline: None,
                content_role: Some(ContextContentRole::Source),
                status: ContextItemStatus::Fresh,
                score: None,
                matched_by: MatchedBy::Lexical,
                metadata: None,
                entity_id: None,
                container: None,
                trace: None,
                query_groups: vec![],
                selection_reason: None,
                coverage_group: None,
            }],
            group_results: vec![],
            diagnostics: ContextDiagnostics::default(),
        }
    }

    fn render(result: &PathLookupResult, detail: bool) -> (String, String) {
        let (mut output, mut diagnostics) = (Vec::new(), Vec::new());
        write_path_lookup_result(
            &mut output,
            &mut diagnostics,
            result,
            OutputOptions {
                preview: if detail {
                    crate::PreviewMode::Detail
                } else {
                    crate::PreviewMode::Short
                },
                color: ColorMode::Never,
                ..OutputOptions::default()
            },
            false,
        )
        .expect("render lookup");
        (
            String::from_utf8(output).expect("UTF-8 output"),
            String::from_utf8(diagnostics).expect("UTF-8 diagnostics"),
        )
    }

    #[test]
    fn terminal_warning_changes_only_diagnostics_not_search_data() {
        let result = PathLookupResult::Text(Box::new(text_result()));
        let expected = render(&result, false).0;
        let mut output = Vec::new();
        let mut diagnostics = Vec::new();
        write_path_lookup_result(
            &mut output,
            &mut diagnostics,
            &result,
            OutputOptions {
                color: ColorMode::Never,
                ..OutputOptions::default()
            },
            true,
        )
        .expect("terminal lookup");
        assert_eq!(String::from_utf8(output).expect("UTF-8"), expected);
        assert_eq!(
            String::from_utf8(diagnostics).expect("UTF-8"),
            "! No matching file paths\n  Searching current file contents literally (rg)\n"
        );
    }

    #[test]
    fn lookup_diagnostics_follow_their_own_terminal_not_stdout() {
        let result = PathLookupResult::Files(PathsResult {
            root: std::env::temp_dir(),
            paths: vec![Path::new("src").join("auth.ts")],
            truncated: true,
        });
        for stdout_terminal in [false, true] {
            for diagnostics_terminal in [false, true] {
                let (mut output, mut diagnostics) = (Vec::new(), Vec::new());
                write_path_lookup_with_terminals(
                    &mut output,
                    &mut diagnostics,
                    &result,
                    OutputOptions {
                        color: ColorMode::Auto,
                        ..OutputOptions::default()
                    },
                    stdout_terminal,
                    diagnostics_terminal,
                )
                .expect("lookup with independent streams");
                assert_eq!(
                    String::from_utf8(output).expect("UTF-8"),
                    "#1 matchedBy=path src/auth.ts\n"
                );
                let diagnostics = String::from_utf8(diagnostics).expect("UTF-8");
                assert_eq!(
                    diagnostics.contains('\x1b'),
                    diagnostics_terminal && std::env::var_os("NO_COLOR").is_none()
                );
                if diagnostics_terminal {
                    assert!(
                        diagnostics
                            .contains("More matching files exist\n  Use --limit to show more\n")
                    );
                } else {
                    assert_eq!(
                        diagnostics,
                        "warning: more matching files exist; use --limit to show more\n"
                    );
                }
            }
        }
    }

    #[test]
    fn file_matches_use_ranked_forward_slash_paths_and_report_truncation_in_both_layouts() {
        let result = PathLookupResult::Files(PathsResult {
            root: std::env::temp_dir(),
            paths: vec![Path::new("src").join("auth.ts")],
            truncated: true,
        });
        for detail in [false, true] {
            assert_eq!(
                render(&result, detail),
                (
                    "#1 matchedBy=path src/auth.ts\n".into(),
                    "warning: more matching files exist; use --limit to show more\n".into(),
                )
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn path_labels_simplify_ordinary_paths_and_preserve_required_windows_syntax() {
        assert_eq!(
            path_label(Path::new(r"\\?\C:\workspace\src\main.rs")),
            "C:/workspace/src/main.rs"
        );
        let long_path = format!(r"\\?\C:\{}main.rs", "folder\\".repeat(50));
        for path in [
            r"\\?\C:\workspace\trailing.",
            r"\\?\C:\workspace\trailing ",
            r"\\?\C:\workspace\NUL.txt",
            r"\\?\UNC\server\share\main.rs",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\main.rs",
            r"\\.\COM1",
            long_path.as_str(),
        ] {
            assert_eq!(path_label(Path::new(path)), path);
        }
    }

    #[test]
    fn text_layout_numbers_every_source_line_and_marks_matches_separately_from_context() {
        let (output, diagnostics) = render(&PathLookupResult::Text(Box::new(text_result())), false);
        assert_eq!(
            output,
            "refs.txt\n  1-\tbefore\n  2:\tImport src/auth.ts\n  3-\tafter\n"
        );
        assert_eq!(
            diagnostics,
            "warning: no matching file paths; searching current file contents literally (rg).\n"
        );
    }

    #[test]
    fn detailed_text_and_empty_results_report_coverage_files_and_match_metadata() {
        let mut result = text_result();
        let (output, diagnostics) = render(&PathLookupResult::Text(Box::new(result.clone())), true);
        assert!(
            output
                .contains(" rg\nQuery: src/auth.ts\nCoverage: rg_exhaustive\nFiles: 1\nHits: 1\n"),
            "{output}"
        );
        assert!(output.contains("File: refs.txt\nHits: 1\n\n  #1 lexical_match matchedBy=lexical\n  Range: 1-3  Status: fresh\n  Source:\n    1-\tbefore\n    2:\tImport src/auth.ts\n    3-\tafter\n"), "{output}");
        assert_eq!(
            diagnostics,
            "warning: no matching file paths; searching current file contents literally (rg).\n"
        );
        result.items.clear();
        let result = PathLookupResult::Text(Box::new(result));
        assert!(
            render(&result, true)
                .0
                .ends_with("Files: 0\nHits: 0\nReason: No matches\n")
        );
        assert_eq!(
            render(&result, false),
            (
                "No matches.\n".into(),
                "warning: no matching file paths; searching current file contents literally (rg).\n"
                    .into()
            )
        );
    }

    #[test]
    fn literal_highlighting_uses_smart_case_and_does_not_highlight_path_components() {
        for (query, source, expected) in [
            (
                "AUTH.ts",
                "AUTH.ts auth.ts AUTHXts ts",
                "\x1b[1;33mAUTH.ts\x1b[0m auth.ts AUTHXts ts",
            ),
            (
                "auth.ts",
                "auth.ts AUTH.ts authXts auth",
                "\x1b[1;33mauth.ts\x1b[0m \x1b[1;33mAUTH.ts\x1b[0m authXts auth",
            ),
            (
                "路径/é.h",
                "路径/É.H 路径/é.h É.H",
                "\x1b[1;33m路径/É.H\x1b[0m \x1b[1;33m路径/é.h\x1b[0m É.H",
            ),
            ("a", "a A", "\x1b[1;33ma\x1b[0m \x1b[1;33mA\x1b[0m"),
        ] {
            assert_eq!(highlight(source, query, true), expected, "{query}");
            assert_eq!(highlight(source, query, false), source);
        }
    }

    #[test]
    fn truncated_text_reports_incomplete_coverage_and_coloring_preserves_unicode_source() {
        let mut result = text_result();
        result.coverage = ContextCoverage::RgTruncated;
        let (output, diagnostics) = render(&PathLookupResult::Text(Box::new(result)), true);
        assert!(output.contains("Coverage: rg_truncated\n"));
        assert!(diagnostics.contains("coverage is incomplete"));
        assert_eq!(
            highlight("路径 src/AUTH.ts", "src/auth.ts", true),
            "路径 \x1b[1;33msrc/AUTH.ts\x1b[0m"
        );
    }
}
