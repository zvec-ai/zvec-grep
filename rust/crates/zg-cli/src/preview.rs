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

/// Keep the fragment's 160-character budget independently of restored line
/// prefixes. Add at most 32 prefix characters and mark any omitted context.
pub(crate) fn short_source_line(item: &ContextItem, offset: usize, line: &str) -> String {
    const SOURCE_CHARS: usize = 160;
    const PREFIX_CHARS: usize = 32;
    if line.chars().take(SOURCE_CHARS + 1).count() <= SOURCE_CHARS {
        return line.to_owned();
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
    let mut chars = line[start..].chars();
    text.extend(
        chars
            .by_ref()
            .take(SOURCE_CHARS + prefix[start..].chars().count()),
    );
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
