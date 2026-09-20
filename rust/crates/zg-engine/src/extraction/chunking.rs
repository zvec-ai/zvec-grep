use crate::utils::utf16_len;

/// Choose a UTF-16 cut position, preferring punctuation near the chunk limit.
pub(super) fn find_line_cut(line: &str, max_chars: usize) -> usize {
    let line_chars = utf16_len(line);
    if line_chars <= max_chars {
        return line_chars;
    }

    let min_position = max_chars.saturating_mul(7) / 10;
    let mut best_position = None;
    let mut best_score = 0;
    let mut position = 0;
    for character in line.chars() {
        if position >= max_chars {
            break;
        }
        let score = match character {
            '.' | '!' | '?' => 4,
            ',' | ';' | ':' => 3,
            ' ' | '\t' => 2,
            '-' | '/' | '\\' => 1,
            _ => 0,
        };
        if position >= min_position && score > 0 && score >= best_score {
            best_score = score;
            best_position = Some(position + character.len_utf16());
        }
        position += character.len_utf16();
    }
    best_position.unwrap_or(max_chars)
}
