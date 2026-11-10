//! Shared source selection and text boundaries for CLI and MCP presentation.

use std::{borrow::Cow, ops::Range};

use zg_engine::api::context::{
    ContextResult,
    result::{
        ContentRange, ContextItem, ContextSource, ContextSourcePreview, ContextSourcePreviewLine,
    },
};

use crate::PreviewMode;

/// Prepares a bounded indexed-search response before it crosses a transport boundary.
///
/// Retrieval and ranking have already completed. Full and detail responses retain
/// their exact stored source; short and minimal responses retain only selected
/// display lines, without pretending those lines form contiguous source text.
pub fn prepare_context_response(result: &mut ContextResult, preview: PreviewMode) {
    if result.source != ContextSource::Index
        || !matches!(preview, PreviewMode::None | PreviewMode::Short)
    {
        return;
    }
    for item in result.items.iter_mut().chain(
        result
            .group_results
            .iter_mut()
            .flat_map(|group| &mut group.items),
    ) {
        if item.source_preview.is_some() {
            continue;
        }
        let source_preview = select_bounded_source(item, preview);
        // Replace instead of clearing so the full entity allocation is released.
        item.content = String::new();
        item.content_range = empty_content_range(&item.content_range);
        item.outline = None;
        item.source_preview = Some(source_preview);
    }
}

fn empty_content_range(range: &ContentRange) -> ContentRange {
    match *range {
        ContentRange::Text {
            start_line,
            start_byte_offset,
            start_byte_column,
            ..
        } => ContentRange::Text {
            start_line,
            end_line: start_line,
            start_byte_offset,
            end_byte_offset: start_byte_offset,
            start_byte_column,
            end_byte_column: start_byte_column,
        },
        ContentRange::Byte { start_offset, .. } => ContentRange::Byte {
            start_offset,
            end_offset: start_offset,
        },
        ContentRange::File => ContentRange::Byte {
            start_offset: 0,
            end_offset: 0,
        },
    }
}

/// Uses assembled lines directly, with the same selector as a compatibility
/// fallback for callers rendering an unprepared engine result in-process.
pub(crate) fn bounded_source_preview(
    item: &ContextItem,
    preview: PreviewMode,
) -> Cow<'_, ContextSourcePreview> {
    match &item.source_preview {
        Some(source) => Cow::Borrowed(source),
        None => Cow::Owned(select_bounded_source(item, preview)),
    }
}

fn select_bounded_source(item: &ContextItem, preview: PreviewMode) -> ContextSourcePreview {
    let first = item.content_range.start_line();
    if preview == PreviewMode::None {
        let source = source_preview(item, Some(1));
        return ContextSourcePreview {
            lines: source
                .window
                .map(|offset| ContextSourcePreviewLine {
                    line_number: first.map(|first| first + offset),
                    text: minimal_source_line(item, offset, source.lines[offset]),
                })
                .collect(),
            trailing_omission: false,
        };
    }
    let lines: Vec<_> = item.content.lines().collect();
    let anchor = item
        .excerpt_range
        .as_ref()
        .unwrap_or(&item.range)
        .start_line()
        .zip(first)
        .map_or(0, |(matched, first)| matched.saturating_sub(first))
        .min(lines.len().saturating_sub(1));
    let from = anchor.saturating_sub(3);
    ContextSourcePreview {
        lines: lines
            .iter()
            .enumerate()
            .skip(from)
            .take(10)
            .map(|(offset, line)| ContextSourcePreviewLine {
                line_number: first.map(|first| first + offset),
                text: short_source_line(item, offset, line),
            })
            .collect(),
        trailing_omission: lines.len() > from + 10,
    }
}

/// Source lines and the selected window, before numbering, markers or highlighting.
pub struct SourcePreview<'a> {
    pub lines: Vec<&'a str>,
    pub window: Range<usize>,
}

/// Selects all lines, a single matched line, or a bounded excerpt with context.
pub fn source_preview(item: &ContextItem, max_lines: Option<usize>) -> SourcePreview<'_> {
    let lines: Vec<_> = if item.content.is_empty() {
        Vec::new()
    } else {
        split_lines(&item.content).collect()
    };
    let anchor = item
        .excerpt_range
        .as_ref()
        .and_then(ContentRange::start_line)
        .zip(item.content_range.start_line())
        .map_or(0, |(line, first)| line.saturating_sub(first));
    let matched_lines = item
        .excerpt_range
        .as_ref()
        .and_then(ContentRange::line_count)
        .unwrap_or(1);
    let window = select_window(lines.len(), anchor, matched_lines, max_lines);
    SourcePreview { lines, window }
}

fn select_window(
    total: usize,
    anchor: usize,
    matched_lines: usize,
    max_lines: Option<usize>,
) -> Range<usize> {
    let limit = max_lines.unwrap_or(total).min(total);
    if limit == 0 || limit == total {
        return 0..limit;
    }
    let anchor = if anchor < total { anchor } else { 0 };
    let before = if limit == 1 || matched_lines >= limit {
        0
    } else {
        2
    };
    let start = anchor.saturating_sub(before).min(total - limit);
    start..start + limit
}

pub fn split_lines(content: &str) -> impl Iterator<Item = &str> {
    // Preserve the final empty source line, matching the Node.js formatter.
    content
        .split_inclusive('\n')
        .map(|line| {
            line.strip_suffix('\n')
                .map_or(line, |line| line.strip_suffix('\r').unwrap_or(line))
        })
        .chain((content.is_empty() || content.ends_with('\n')).then_some(""))
}

/// Keep the fragment's 160-character budget independently of restored line
/// prefixes. Add at most 32 prefix characters and mark any omitted context.
pub(crate) fn short_source_line(item: &ContextItem, offset: usize, line: &str) -> String {
    bounded_source_line(item, offset, line, truncate_chars)
}

/// The single-line summary keeps its existing UTF-16 budget while using the
/// same evidence window as the compact preview.
pub(crate) fn minimal_source_line(item: &ContextItem, offset: usize, line: &str) -> String {
    bounded_source_line(item, offset, line, truncate_utf16)
}

fn bounded_source_line(
    item: &ContextItem,
    offset: usize,
    line: &str,
    truncate: fn(&str, usize) -> String,
) -> String {
    const SOURCE_CHARS: usize = 160;
    const PREFIX_CHARS: usize = 32;
    let ordinary = truncate(line, SOURCE_CHARS);
    if ordinary == line {
        return ordinary;
    }
    let prefix_bytes = match (&item.excerpt_range, &item.content_range) {
        (
            Some(ContentRange::Text {
                start_line,
                start_byte_column,
                ..
            }),
            ContentRange::Text {
                start_line: first,
                start_byte_column: first_column,
                ..
            },
        ) if first.checked_add(offset) == Some(*start_line) => {
            start_byte_column.saturating_sub(if offset == 0 { *first_column } else { 0 })
        }
        _ => 0,
    };
    // Invalid or mismatched coordinates retain the normal line-prefix preview.
    let prefix = line.get(..prefix_bytes).unwrap_or_default();
    if prefix.is_empty() {
        return ordinary;
    }
    let start = prefix
        .char_indices()
        .rev()
        .nth(PREFIX_CHARS - 1)
        .map_or(0, |(start, _)| start);
    let mut text = if start == 0 {
        String::new()
    } else {
        "...".to_owned()
    };
    text.push_str(&prefix[start..]);
    text.push_str(&truncate(&line[prefix.len()..], SOURCE_CHARS));
    text
}

fn truncate_chars(line: &str, max: usize) -> String {
    let mut chars = line.chars();
    let mut text: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        text.push_str("...");
    }
    text
}

/// Applies the Node.js UTF-16 length limit without splitting a Unicode scalar.
#[must_use]
pub fn truncate_utf16(value: &str, max: usize) -> String {
    if value.encode_utf16().count() <= max {
        return value.to_owned();
    }
    let mut units = 0;
    let prefix: String = value
        .chars()
        .take_while(|character| {
            units += character.len_utf16();
            units < max
        })
        .collect();
    format!("{prefix}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zg_engine::api::context::result::{
        ContextContentRole, ContextCoverage, ContextDiagnostics, ContextGroupResult,
        ContextItemKind, ContextItemStatus, ContextQueryGroupRole, MatchedBy,
    };

    fn response_fixture() -> ContextResult {
        let prefix = format!("prefix {}", "中".repeat(200));
        let line = format!("{prefix}needle😀{}", "尾".repeat(200));
        let content = format!("{}\r\ntail-sentinel\r\n", format!("{line}\r\n").repeat(30));
        let range = ContentRange::Text {
            start_line: 17,
            end_line: 49,
            start_byte_offset: 100,
            end_byte_offset: 100 + content.len(),
            start_byte_column: 3,
            end_byte_column: 0,
        };
        let matched_start = 100 + (line.len() + 2) * 8 + prefix.len();
        let item = ContextItem {
            kind: ContextItemKind::IndexedEntity,
            rank: 1,
            absolute_path: std::env::temp_dir().join("entity.md"),
            relative_path: "entity.md".into(),
            range: range.clone(),
            excerpt_range: Some(ContentRange::Text {
                start_line: 25,
                end_line: 25,
                start_byte_offset: matched_start,
                end_byte_offset: matched_start + "needle😀".len(),
                start_byte_column: prefix.len(),
                end_byte_column: prefix.len() + "needle😀".len(),
            }),
            content_range: range,
            match_ranges: Vec::new(),
            content,
            source_preview: None,
            outline: Some("source outline".into()),
            content_role: Some(ContextContentRole::Source),
            status: ContextItemStatus::Fresh,
            score: Some(0.75),
            matched_by: MatchedBy::Fts,
            metadata: None,
            entity_id: Some("entity-id".into()),
            container: None,
            trace: None,
            query_groups: Vec::new(),
            selection_reason: None,
            coverage_group: None,
        };
        ContextResult {
            query: "needle".into(),
            freshness: None,
            background_refresh: None,
            indexing: None,
            root: std::env::temp_dir(),
            source: ContextSource::Index,
            coverage: ContextCoverage::RankedSample,
            workspace_index: None,
            items: vec![item.clone()],
            group_results: vec![ContextGroupResult {
                id: "Q1".into(),
                query: "needle".into(),
                role: ContextQueryGroupRole::Primary,
                items: vec![item],
            }],
            diagnostics: ContextDiagnostics::default(),
        }
    }

    fn render(result: &ContextResult, preview: PreviewMode) -> Vec<u8> {
        let mut output = Vec::new();
        crate::write_context_with_options(
            &mut output,
            result,
            crate::OutputOptions {
                preview,
                color: crate::ColorMode::Never,
                ..crate::OutputOptions::default()
            },
            false,
        )
        .expect("render context");
        output
    }

    #[test]
    fn short_response_bounds_unicode_lines_and_drops_both_full_source_copies() {
        let mut result = response_fixture();
        let original = result.clone();
        let expected_output = render(&result, PreviewMode::Short);
        let original_wire = serde_json::to_vec(&result).expect("full response");
        prepare_context_response(&mut result, PreviewMode::Short);
        for item in [&result.items[0], &result.group_results[0].items[0]] {
            assert!(item.content.is_empty());
            assert_eq!(item.content.capacity(), 0, "release the entity allocation");
            assert!(item.outline.is_none());
            assert_eq!(item.range, original.items[0].range);
            assert_eq!(item.excerpt_range, original.items[0].excerpt_range);
            assert_eq!(
                item.content_range,
                ContentRange::Text {
                    start_line: 17,
                    end_line: 17,
                    start_byte_offset: 100,
                    end_byte_offset: 100,
                    start_byte_column: 3,
                    end_byte_column: 3,
                }
            );
            let source = item.source_preview.as_ref().expect("assembled source");
            assert_eq!(source.lines.len(), 10);
            assert_eq!(source.lines[0].line_number, Some(22));
            assert_eq!(source.lines[9].line_number, Some(31));
            assert!(source.lines[3].text.contains("needle😀"));
            assert!(
                source
                    .lines
                    .iter()
                    .all(|line| { line.text.chars().count() <= 198 && !line.text.contains('\r') })
            );
            assert!(source.trailing_omission);
        }
        assert_eq!(render(&result, PreviewMode::Short), expected_output);
        let wire = serde_json::to_vec(&result).expect("bounded response");
        assert!(wire.len() < original_wire.len() / 4);
        assert!(
            !String::from_utf8(wire)
                .expect("JSON")
                .contains("tail-sentinel")
        );
        let prepared = result.clone();
        prepare_context_response(&mut result, PreviewMode::Short);
        assert_eq!(result, prepared, "assembly is idempotent");
    }

    #[test]
    fn minimal_response_keeps_one_matched_line_and_the_utf16_budget() {
        let mut result = response_fixture();
        let expected_output = render(&result, PreviewMode::None);
        prepare_context_response(&mut result, PreviewMode::None);
        let source = result.items[0].source_preview.as_ref().expect("summary");
        assert_eq!(source.lines.len(), 1);
        assert_eq!(source.lines[0].line_number, Some(25));
        assert!(source.lines[0].text.contains("needle😀"));
        assert!(source.lines[0].text.encode_utf16().count() <= 198);
        assert!(!source.trailing_omission);
        assert!(result.items[0].content.is_empty());
        assert!(result.group_results[0].items[0].content.is_empty());
        assert_eq!(render(&result, PreviewMode::None), expected_output);
    }

    #[test]
    fn complete_responses_preserve_stored_bytes_and_exact_coordinates() {
        let original = response_fixture();
        for preview in [PreviewMode::Full, PreviewMode::Detail] {
            let mut result = original.clone();
            prepare_context_response(&mut result, preview);
            assert_eq!(result, original);
        }
        let mut lexical = original;
        lexical.source = ContextSource::Rg;
        let original = lexical.clone();
        prepare_context_response(&mut lexical, PreviewMode::Short);
        assert_eq!(
            lexical, original,
            "rg retains its independent context contract"
        );
    }

    #[test]
    fn line_boundaries_preserve_empty_lines_and_only_strip_cr_before_lf() {
        assert_eq!(split_lines("").collect::<Vec<_>>(), [""]);
        assert_eq!(
            split_lines("a\r\n\r\nb\r\n").collect::<Vec<_>>(),
            ["a", "", "b", ""]
        );
        assert_eq!(split_lines("a\nb\r").collect::<Vec<_>>(), ["a", "b\r"]);
    }

    #[test]
    fn windows_keep_matches_in_view_and_bound_context() {
        for (total, anchor, matched, limit, expected) in [
            (0, 0, 1, Some(10), 0..0),
            (5, 3, 1, Some(10), 0..5),
            (20, 8, 1, Some(10), 6..16),
            (20, 8, 10, Some(10), 8..18),
            (20, 19, 1, Some(10), 10..20),
            (20, 20, 1, Some(10), 0..10),
            (20, 8, 1, Some(1), 8..9),
            (20, 20, 1, Some(1), 0..1),
            (20, 8, 1, None, 0..20),
        ] {
            assert_eq!(select_window(total, anchor, matched, limit), expected);
        }
    }

    #[test]
    fn truncation_counts_utf16_without_splitting_surrogate_pairs() {
        let exact = format!("{}😀", "a".repeat(158));
        assert_eq!(truncate_utf16(&exact, 160), exact);
        assert_eq!(
            truncate_utf16(&format!("{exact}b"), 160),
            format!("{}...", "a".repeat(158))
        );
        assert_eq!(
            truncate_utf16(&format!("{}😀bc", "a".repeat(157)), 160),
            format!("{}😀...", "a".repeat(157))
        );
    }
}
