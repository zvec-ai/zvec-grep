//! Shared source selection and text boundaries for CLI and MCP presentation.

use std::ops::Range;

use zg_engine::api::context::result::{ContentRange, ContextItem};

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
