use crate::{
    EngineError, EngineResult,
    domain::{Range, TextRange},
};

pub(crate) fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// Takes at most `count` UTF-16 units without splitting a character.
pub(crate) fn take_utf16(value: &str, count: usize) -> &str {
    &value[..byte_offset_at_utf16_floor(value, count)]
}

/// Rounds down to a character boundary; offsets past the end are clamped.
pub(crate) fn byte_offset_at_utf16_floor(value: &str, utf16_offset: usize) -> usize {
    let mut units = 0;
    for (index, character) in value.char_indices() {
        let next = units + character.len_utf16();
        if next > utf16_offset {
            return index;
        }
        units = next;
    }
    value.len()
}

/// Rounds up to a character boundary; offsets past the end are clamped.
pub(crate) fn byte_offset_at_utf16_ceil(value: &str, utf16_offset: usize) -> usize {
    let mut units = 0;
    for (index, character) in value.char_indices() {
        if units >= utf16_offset {
            return index;
        }
        units += character.len_utf16();
        if units > utf16_offset {
            return index + character.len_utf8();
        }
    }
    value.len()
}

/// Returns UTF-8 byte line starts, including the empty line after a final `\n`.
pub(crate) fn line_byte_offsets(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
        .collect()
}

pub(crate) fn collapse_whitespace(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for word in value.split_whitespace() {
        if !output.is_empty() {
            output.push(' ');
        }
        output.push_str(word);
    }
    output
}

/// Reads a UTF-8 span without copying the source text.
pub(crate) fn slice_text(text: &str, start: usize, end: usize) -> EngineResult<&str> {
    text.get(start..end).ok_or_else(|| {
        EngineError::invalid_argument(format!(
            "cannot read text range {start}..{end}: offsets must be ordered, within the decoded source ({} bytes), and on UTF-8 character boundaries",
            text.len()
        ))
    })
}

/// Maps offsets into decoded source text. `line_starts` must list every line's
/// byte offset, sorted and starting with 0, as produced by `line_byte_offsets`.
pub(crate) fn text_range_from_offsets(
    text: &str,
    line_starts: &[usize],
    start: usize,
    end: usize,
) -> EngineResult<TextRange> {
    slice_text(text, start, end)?;
    let position = |offset| {
        let line = line_starts.partition_point(|&start| start <= offset);
        let line_start = *line_starts.get(line.checked_sub(1)?)?;
        if line_starts.first() != Some(&0)
            || (line_start > 0 && text.as_bytes().get(line_start - 1) != Some(&b'\n'))
        {
            return None;
        }
        Some((line, offset - line_start))
    };
    let (Some((start_line, start_column)), Some((end_line, end_column))) =
        (position(start), position(end))
    else {
        return Err(EngineError::invalid_argument(
            "cannot create text range: line starts must belong to the decoded source",
        ));
    };
    TextRange::from_coordinates(start, end, start_line, end_line, start_column, end_column)
}

/// Maps local text coordinates into a containing source range.
pub(crate) fn map_text_range(local: TextRange, origin: TextRange) -> EngineResult<TextRange> {
    let translate = |offset: usize, line: usize, column: usize| {
        Some((
            origin.start_byte_offset().checked_add(offset)?,
            origin.start_line().checked_add(line - 1)?,
            if line == 1 {
                origin.start_byte_column().checked_add(column)?
            } else {
                column
            },
        ))
    };
    let mapped = translate(
        local.start_byte_offset(),
        local.start_line(),
        local.start_byte_column(),
    )
    .zip(translate(
        local.end_byte_offset(),
        local.end_line(),
        local.end_byte_column(),
    ))
    .ok_or_else(|| EngineError::invalid_argument("text range coordinate overflow"))?;
    let ((start, start_line, start_column), (end, end_line, end_column)) = mapped;
    let mapped =
        TextRange::from_coordinates(start, end, start_line, end_line, start_column, end_column)?;
    if !Range::Text(origin).contains(&Range::Text(mapped))?
        || mapped.end_line() > origin.end_line()
        || (mapped.end_line() == origin.end_line()
            && mapped.end_byte_column() > origin.end_byte_column())
    {
        return Err(EngineError::invalid_argument(
            "local text range lies outside source range",
        ));
    }
    Ok(mapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_text_operations() {
        for (text, length, units, bytes, prefix) in [
            ("hello", 5, 2, 2, "he"),
            ("中文", 2, 1, 3, "中"),
            ("A😀中B", 5, 3, 5, "A😀"),
            ("e\u{301}", 2, 1, 1, "e"),
        ] {
            assert_eq!(utf16_len(text), length, "{text:?}");
            assert_eq!(byte_offset_at_utf16_floor(text, units), bytes, "{text:?}");
            assert_eq!(byte_offset_at_utf16_ceil(text, units), bytes, "{text:?}");
            assert_eq!(take_utf16(text, units), prefix, "{text:?}");
            assert_eq!(
                slice_text(text, 0, bytes).expect("valid prefix"),
                prefix,
                "{text:?}"
            );
        }
        for (text, starts, start, end, start_column, end_column, content) in [
            ("ab\ncd", [0, 3], 1, 4, 1, 1, "b\nc"),
            ("a中\r\n😀z", [0, 6], 1, 10, 1, 4, "中\r\n😀"),
        ] {
            assert_eq!(line_byte_offsets(text), starts, "{text:?}");
            assert_eq!(
                text_range_from_offsets(text, &starts, start, end).expect("source coordinates"),
                TextRange::from_coordinates(start, end, 1, 2, start_column, end_column)
                    .expect("expected coordinates"),
                "{text:?}",
            );
            assert_eq!(
                slice_text(text, start, end).expect("source slice"),
                content,
                "{text:?}"
            );
        }
        let origin = TextRange::from_coordinates(100, 120, 10, 12, 4, 0).expect("source range");
        for (end, end_line, end_column, expected_end_line, expected_end_column) in
            [(5, 1, 5, 10, 9), (8, 2, 1, 11, 1)]
        {
            let local = TextRange::from_coordinates(2, end, 1, end_line, 2, end_column)
                .expect("local range");
            assert_eq!(
                map_text_range(local, origin).expect("mapped range"),
                TextRange::from_coordinates(
                    102,
                    100 + end,
                    10,
                    expected_end_line,
                    6,
                    expected_end_column,
                )
                .expect("expected coordinates"),
                "{local:?}",
            );
        }
        assert_eq!(collapse_whitespace("alpha\tbeta\n中"), "alpha beta 中");
    }

    #[test]
    fn utf16_boundary_cases() {
        let text = "A😀中B";
        for (offset, floor, ceil) in [
            (0, 0, 0),
            (1, 1, 1),
            (2, 1, 5),
            (3, 5, 5),
            (4, 8, 8),
            (5, 9, 9),
            (6, 9, 9),
            (usize::MAX, 9, 9),
        ] {
            assert_eq!(byte_offset_at_utf16_floor(text, offset), floor, "{offset}");
            assert_eq!(byte_offset_at_utf16_ceil(text, offset), ceil, "{offset}");
            assert_eq!(take_utf16(text, offset), &text[..floor], "{offset}");
        }
        for (offset, floor, ceil, prefix) in [(1, 0, 4, ""), (3, 4, 8, "😀")] {
            assert_eq!(
                byte_offset_at_utf16_floor("😀😀", offset),
                floor,
                "{offset}"
            );
            assert_eq!(byte_offset_at_utf16_ceil("😀😀", offset), ceil, "{offset}");
            assert_eq!(take_utf16("😀😀", offset), prefix, "{offset}");
        }
        assert_eq!(utf16_len(""), 0);
        for offset in [0, 1, usize::MAX] {
            assert_eq!(byte_offset_at_utf16_floor("", offset), 0, "{offset}");
            assert_eq!(byte_offset_at_utf16_ceil("", offset), 0, "{offset}");
            assert_eq!(take_utf16("", offset), "", "{offset}");
        }
    }

    #[test]
    fn slice_boundary_cases() {
        for (text, start, end, expected) in [
            ("", 0, 0, ""),
            ("a中😀", 0, 8, "a中😀"),
            ("a中😀", 0, 0, ""),
            ("a中😀", 4, 4, ""),
            ("a中😀", 8, 8, ""),
        ] {
            assert_eq!(
                slice_text(text, start, end).expect("valid slice"),
                expected,
                "{text:?}, {start}..{end}",
            );
            assert!(
                text_range_from_offsets(text, &line_byte_offsets(text), start, end).is_ok(),
                "{text:?}, {start}..{end}",
            );
        }
        let text = "a中😀";
        let starts = line_byte_offsets(text);
        for (start, end) in [
            (2, 4),
            (1, 3),
            (5, 8),
            (4, 7),
            (2, 2),
            (4, 1),
            (0, 9),
            (9, 9),
            (0, usize::MAX),
            (usize::MAX, usize::MAX),
        ] {
            assert!(slice_text(text, start, end).is_err(), "{start}..{end}");
            assert!(
                text_range_from_offsets(text, &starts, start, end).is_err(),
                "{start}..{end}",
            );
        }
        assert!(slice_text("", 0, 1).is_err());
        assert!(slice_text("", 1, 1).is_err());
    }

    #[test]
    fn line_boundary_cases() {
        for (text, starts, positions) in [
            (
                "",
                &[0usize] as &[usize],
                &[(0usize, 1usize, 0usize)] as &[(usize, usize, usize)],
            ),
            ("a😀", &[0], &[(0, 1, 0), (1, 1, 1), (5, 1, 5)]),
            ("\n", &[0, 1], &[(0, 1, 0), (1, 2, 0)]),
            ("\n\n", &[0, 1, 2], &[(0, 1, 0), (1, 2, 0), (2, 3, 0)]),
            ("a\nb", &[0, 2], &[(1, 1, 1), (2, 2, 0), (3, 2, 1)]),
            (
                "a\r\nb\r\n",
                &[0, 3, 6],
                &[(1, 1, 1), (2, 1, 2), (3, 2, 0), (5, 2, 2), (6, 3, 0)],
            ),
            ("a\rb", &[0], &[(1, 1, 1), (2, 1, 2), (3, 1, 3)]),
            (
                "A😀\r\n中\n\n",
                &[0, 7, 11, 12],
                &[
                    (1, 1, 1),
                    (5, 1, 5),
                    (6, 1, 6),
                    (7, 2, 0),
                    (10, 2, 3),
                    (11, 3, 0),
                    (12, 4, 0),
                ],
            ),
        ] {
            assert_eq!(line_byte_offsets(text), starts, "{text:?}");
            for &(offset, line, column) in positions {
                assert_eq!(
                    text_range_from_offsets(text, starts, offset, offset)
                        .expect("boundary coordinates"),
                    TextRange::from_coordinates(offset, offset, line, line, column, column)
                        .expect("expected coordinates"),
                    "{text:?}, offset {offset}",
                );
            }
        }
        let text = "a中\r\n😀\n";
        assert_eq!(
            text_range_from_offsets(text, &line_byte_offsets(text), 6, 11)
                .expect("half-open range"),
            TextRange::from_coordinates(6, 11, 2, 3, 0, 0).expect("expected coordinates"),
        );
        assert_eq!(
            slice_text(text, 6, 11).expect("slice ending at next line"),
            "😀\n"
        );
    }

    #[test]
    fn invalid_line_starts() {
        let text = "a\nb\nc";
        for (starts, start, end) in [(&[] as &[usize], 0, 1), (&[2, 4], 2, 3), (&[0, 1], 1, 3)] {
            assert!(
                text_range_from_offsets(text, starts, start, end).is_err(),
                "line starts {starts:?}, {start}..{end}",
            );
        }
    }

    #[test]
    fn whitespace_boundary_cases() {
        for (text, expected) in [
            ("", ""),
            ("already spaced", "already spaced"),
            ("  alpha  ", "alpha"),
            ("alpha   beta", "alpha beta"),
            (" \talpha\r\n\u{2003}中\u{a0}😀  ", "alpha 中 😀"),
            (" \n\t\u{2003}", ""),
            ("a\u{85}\u{2028}\u{2029}\u{b}\u{c}b", "a b"),
            ("alpha\u{200b}beta", "alpha\u{200b}beta"),
        ] {
            assert_eq!(collapse_whitespace(text), expected, "{text:?}");
        }
    }

    #[test]
    fn range_mapping_boundary_cases() {
        // Entity content "abcd\nx\nwxyz" starts at source line 3, byte column 2.
        let origin = TextRange::from_coordinates(10, 21, 3, 5, 2, 4).expect("source range");
        for (local, expected) in [
            ((0, 11, 1, 3, 0, 4), (10, 21, 3, 5, 2, 4)),
            ((0, 0, 1, 1, 0, 0), (10, 10, 3, 3, 2, 2)),
            ((5, 11, 2, 3, 0, 4), (15, 21, 4, 5, 0, 4)),
            ((11, 11, 3, 3, 4, 4), (21, 21, 5, 5, 4, 4)),
        ] {
            let (start, end, start_line, end_line, start_column, end_column) = local;
            let local = TextRange::from_coordinates(
                start,
                end,
                start_line,
                end_line,
                start_column,
                end_column,
            )
            .expect("local range");
            let (start, end, start_line, end_line, start_column, end_column) = expected;
            assert_eq!(
                map_text_range(local, origin).expect("mapped range"),
                TextRange::from_coordinates(
                    start,
                    end,
                    start_line,
                    end_line,
                    start_column,
                    end_column
                )
                .expect("expected coordinates"),
                "{local:?}",
            );
        }
        for (end, end_line, end_column) in [
            (12, 3, 4), // Past the origin's byte range.
            (10, 4, 0), // Within its byte range, but past its final line.
            (11, 3, 5), // Within its byte range, but past its final column.
        ] {
            let local = TextRange::from_coordinates(0, end, 1, end_line, 0, end_column)
                .expect("local range");
            assert!(map_text_range(local, origin).is_err(), "{local:?}");
        }
        let local = TextRange::from_coordinates(0, 0, 1, 1, 0, 0).expect("empty local range");
        let origin = TextRange::from_coordinates(0, 0, 1, 1, 0, 0).expect("empty source range");
        assert_eq!(
            map_text_range(local, origin).expect("empty mapped range"),
            origin
        );
    }

    #[test]
    fn range_mapping_overflow() {
        let origin = TextRange::from_coordinates(usize::MAX - 2, usize::MAX, 2, 2, 0, 2)
            .expect("source ending at usize max");
        let local = TextRange::from_coordinates(0, 2, 1, 1, 0, 2).expect("local range");
        assert_eq!(map_text_range(local, origin).expect("no overflow"), origin);
        for (start, end) in [(0, 3), (3, 3)] {
            let local =
                TextRange::from_coordinates(start, end, 1, 1, start, end).expect("local range");
            assert!(map_text_range(local, origin).is_err(), "{local:?}");
        }
        let origin =
            TextRange::from_coordinates(usize::MAX - 1, usize::MAX, usize::MAX, usize::MAX, 0, 1)
                .expect("source at maximum line number");
        let local = TextRange::from_coordinates(0, 1, 1, 2, 0, 0).expect("local newline");
        assert!(map_text_range(local, origin).is_err());
    }
}
