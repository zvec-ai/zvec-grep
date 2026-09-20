use crate::utils::utf16_len;

use super::ExtractedEntityFragment;
use crate::domain::{ByteRange, Range};
use crate::utils::{byte_offset_at_utf16_ceil, byte_offset_at_utf16_floor};

/// Select exact source ranges, retaining delimiters and surrounding whitespace.
/// Whitespace-only ranges remain in the entity without creating embedding inputs.
pub(super) fn text_fragments(
    text: &str,
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<ExtractedEntityFragment> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    if utf16_len(text) <= max_chars {
        return vec![ExtractedEntityFragment { range: Range::Full }];
    }
    let mut fragments = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let rest = &text[start..];
        let mut count = byte_offset_at_utf16_floor(rest, max_chars);
        if count == 0 {
            count = byte_offset_at_utf16_ceil(rest, max_chars);
        }
        if count < rest.len()
            && let Some(newline) = rest[..count].rfind('\n')
            && utf16_len(&rest[..=newline]) >= max_chars.saturating_mul(7) / 10
        {
            count = newline + 1;
        }
        let end = start + count;
        if !text[start..end].trim().is_empty() {
            fragments.push(ExtractedEntityFragment {
                range: Range::Byte(ByteRange {
                    start_offset: start as u64,
                    end_offset: end as u64,
                }),
            });
        }
        if end == text.len() {
            break;
        }
        let current = &text[start..end];
        let next = byte_offset_at_utf16_ceil(
            current,
            utf16_len(current).saturating_sub(overlap_chars).max(1),
        );
        start += next;
    }
    fragments
}

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
