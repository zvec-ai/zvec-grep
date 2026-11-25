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
                range: Range::Byte(
                    ByteRange::new(start as u64, end as u64).expect("ordered byte offsets"),
                ),
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

/// Refine an extracted entity's existing fragment boundaries for a smaller
/// owning runtime. All output offsets remain relative to the original entity.
pub(crate) fn limit_entity_fragments(
    entity: &mut super::ExtractedEntity,
    options: super::ChunkOptions,
) -> Result<(), crate::EngineError> {
    let Some(maximum) = options.max_chunk_chars else {
        return Ok(());
    };
    let (crate::domain::Content::Code(text) | crate::domain::Content::Text(text)) = &entity.content
    else {
        return Ok(());
    };
    let (maximum, overlap) = super::chunk_options_for_metadata(
        maximum,
        options.chunk_overlap_chars.unwrap_or(0),
        entity.metadata.as_ref(),
    );
    let mut limited = Vec::new();
    for fragment in &entity.fragments {
        let (base, selected) = match fragment.range {
            Range::Full => (0, text.as_str()),
            Range::Byte(range) => {
                let start = usize::try_from(range.start_offset()).map_err(|_| {
                    crate::EngineError::invalid_argument("fragment start exceeds platform limits")
                })?;
                let end = usize::try_from(range.end_offset()).map_err(|_| {
                    crate::EngineError::invalid_argument("fragment end exceeds platform limits")
                })?;
                (
                    range.start_offset(),
                    crate::utils::slice_text(text, start, end)?,
                )
            }
            Range::Text(_) | Range::Pdf(_) => {
                return Err(crate::EngineError::invalid_argument(
                    "expected an entity-relative text fragment",
                ));
            }
        };
        if utf16_len(selected) <= maximum {
            limited.push(fragment.clone());
            continue;
        }
        for part in text_fragments(selected, maximum, overlap) {
            if let Range::Byte(range) = part.range {
                limited.push(ExtractedEntityFragment {
                    range: Range::Byte(ByteRange::new(
                        base + range.start_offset(),
                        base + range.end_offset(),
                    )?),
                });
            } else {
                limited.push(fragment.clone());
            }
        }
    }
    entity.fragments = limited;
    Ok(())
}

/// Choose a UTF-16 cut position, preferring punctuation near the chunk limit.
pub(super) fn find_line_cut(line: &str, max_chars: usize) -> usize {
    let min_position = max_chars.saturating_mul(7) / 10;
    let mut best_position = None;
    let mut best_score = 0;
    let mut position = 0;
    for character in line.chars() {
        if position >= max_chars {
            return best_position.unwrap_or(max_chars);
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
    if position <= max_chars {
        position
    } else {
        best_position.unwrap_or(max_chars)
    }
}
