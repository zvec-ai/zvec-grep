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

/// Returns UTF-8 byte line starts for lines produced by `split('\n')`, retaining any `\r`.
pub(crate) fn line_byte_offsets(lines: &[&str]) -> Vec<usize> {
    let mut offset = 0;
    lines
        .iter()
        .map(|line| {
            let current = offset;
            offset += line.len() + 1;
            current
        })
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
    fn source_coordinates_and_slices_preserve_unicode_crlf_and_empty_spans() {
        let text = "a中\r\n😀\n";
        let starts = line_byte_offsets(&text.split('\n').collect::<Vec<_>>());
        for (start, end, start_line, end_line, start_column, end_column, expected) in [
            (1, 10, 1, 2, 1, 4, "中\r\n😀"),
            (6, 11, 2, 3, 0, 0, "😀\n"),
            (11, 11, 3, 3, 0, 0, ""),
        ] {
            let range = text_range_from_offsets(text, &starts, start, end).expect("source range");
            assert_eq!(
                range,
                TextRange::from_coordinates(
                    start,
                    end,
                    start_line,
                    end_line,
                    start_column,
                    end_column
                )
                .expect("expected coordinates"),
            );
            assert_eq!(
                slice_text(text, start, end).expect("source slice"),
                expected
            );
        }
        for (start, end) in [(2, 10), (1, 9), (0, 12), (10, 1)] {
            assert!(slice_text(text, start, end).is_err());
            assert!(text_range_from_offsets(text, &starts, start, end).is_err());
        }
        assert!(text_range_from_offsets(text, &[], 0, 1).is_err());
        assert!(text_range_from_offsets(text, &[0, 1], 1, 4).is_err());
    }

    #[test]
    fn utf16_offsets_respect_character_boundaries_and_clamp_to_the_end() {
        let text = "A😀中B";
        assert_eq!(utf16_len(text), 5);
        for (offset, floor, ceil) in [
            (0, 0, 0),
            (1, 1, 1),
            (2, 1, 5),
            (3, 5, 5),
            (4, 8, 8),
            (5, 9, 9),
            (usize::MAX, 9, 9),
        ] {
            assert_eq!(byte_offset_at_utf16_floor(text, offset), floor);
            assert_eq!(byte_offset_at_utf16_ceil(text, offset), ceil);
            assert_eq!(take_utf16(text, offset), &text[..floor]);
        }
        assert_eq!(utf16_len(""), 0);
        assert_eq!(take_utf16("", usize::MAX), "");
        assert_eq!(byte_offset_at_utf16_ceil("", usize::MAX), 0);
    }

    #[test]
    fn line_offsets_count_crlf_and_empty_lines() {
        let lines = "A😀\r\n中\n\n".split('\n').collect::<Vec<_>>();
        assert_eq!(line_byte_offsets(&lines), [0, 7, 11, 12]);
        assert_eq!(line_byte_offsets(&["a", "😀"]), [0, 2]);
        assert_eq!(line_byte_offsets(&[""]), [0]);
        assert!(line_byte_offsets(&[]).is_empty());
    }

    #[test]
    fn whitespace_is_trimmed_and_collapsed_across_unicode_and_line_breaks() {
        assert_eq!(
            collapse_whitespace(" \talpha\r\n\u{2003}中\u{a0}😀  "),
            "alpha 中 😀"
        );
        assert_eq!(collapse_whitespace(" \n\t\u{2003}"), "");
        assert_eq!(collapse_whitespace("already spaced"), "already spaced");
    }
}
