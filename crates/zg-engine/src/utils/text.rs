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

/// Returns UTF-16 line starts for lines produced by `split('\n')`, retaining any `\r`.
pub(crate) fn utf16_line_offsets(lines: &[&str]) -> Vec<usize> {
    let mut offset = 0;
    lines
        .iter()
        .map(|line| {
            let current = offset;
            offset += utf16_len(line) + 1;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(utf16_line_offsets(&lines), [0, 5, 7, 8]);
        assert_eq!(utf16_line_offsets(&["a", "😀"]), [0, 2]);
        assert_eq!(utf16_line_offsets(&[""]), [0]);
        assert!(utf16_line_offsets(&[]).is_empty());
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
