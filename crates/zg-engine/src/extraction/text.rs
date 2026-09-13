use crate::{
    EngineError,
    domain::{Content, Entity, EntityContent, SourceRange},
    utils::{byte_offset_at_utf16_ceil, utf16_len, utf16_line_offsets},
};

use super::{
    ChunkOptions, EntityFragment, TextRange, TextSource, make_entity_id, validate_source_file,
};

const DEFAULT_TEXT_CHUNK_CHARS: usize = 3_600;
const DEFAULT_TEXT_CHUNK_OVERLAP_CHARS: usize = 540;

pub(super) fn extract(
    source: &TextSource,
    options: ChunkOptions,
) -> Result<Vec<EntityFragment>, EngineError> {
    validate_source_file(&source.file)?;
    let (max_chars, overlap_chars) = resolve_options(options)?;
    Ok(extract_plain_text_fragments(
        source,
        max_chars,
        overlap_chars,
    ))
}

pub(super) fn extract_plain_text_fragments(
    source: &TextSource,
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<EntityFragment> {
    chunk_text(&source.text, max_chars, overlap_chars)
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            EntityFragment::Standalone(Entity {
                id: make_entity_id(&source.file.id, index),
                file_id: source.file.id.clone(),
                range: SourceRange::Text(chunk.range),
                content: EntityContent::Source(vec![Content::Text(chunk.text)]),
                metadata: None,
            })
        })
        .collect()
}

fn resolve_options(options: ChunkOptions) -> Result<(usize, usize), EngineError> {
    let max_chars = options.max_chunk_chars.unwrap_or(DEFAULT_TEXT_CHUNK_CHARS);
    let overlap_chars = options
        .chunk_overlap_chars
        .unwrap_or(DEFAULT_TEXT_CHUNK_OVERLAP_CHARS);
    if max_chars == 0 {
        return Err(EngineError::invalid_argument(
            "text extractor requires a positive integer chunk size",
        ));
    }
    if overlap_chars >= max_chars {
        return Err(EngineError::invalid_argument(
            "text extractor requires overlap to be smaller than chunk size",
        ));
    }
    Ok((max_chars, overlap_chars))
}

#[derive(Debug)]
struct TextChunk {
    text: String,
    range: TextRange,
}

fn chunk_text(text: &str, max_chars: usize, overlap_chars: usize) -> Vec<TextChunk> {
    if text.trim().is_empty() {
        return Vec::new();
    }

    let lines = text.split('\n').collect::<Vec<_>>();
    let line_offsets = utf16_line_offsets(&lines);
    let mut chunks = Vec::new();
    let mut start_index = 0;

    while start_index < lines.len() {
        if utf16_len(lines[start_index]) + 1 > max_chars {
            split_long_line(
                lines[start_index],
                start_index,
                line_offsets[start_index],
                max_chars,
                &mut chunks,
            );
            start_index += 1;
            continue;
        }

        let mut used_chars = 0;
        let mut end_index = start_index;
        while end_index < lines.len() {
            let line_length = utf16_len(lines[end_index]) + 1;
            if used_chars + line_length > max_chars && end_index > start_index {
                break;
            }
            used_chars += line_length;
            end_index += 1;
        }

        let chunk = lines[start_index..end_index].join("\n");
        if !chunk.trim().is_empty() {
            let end_line_index = end_index - 1;
            chunks.push(TextChunk {
                text: chunk,
                range: TextRange {
                    start_line: start_index + 1,
                    end_line: end_index,
                    start_utf16_offset: line_offsets[start_index],
                    end_utf16_offset: line_offsets[end_line_index]
                        + utf16_len(lines[end_line_index]),
                },
            });
        }

        if end_index >= lines.len() {
            break;
        }
        start_index = compute_next_start_line(&lines, start_index, end_index, overlap_chars);
    }

    chunks
}

fn split_long_line(
    line: &str,
    line_index: usize,
    line_offset: usize,
    max_chars: usize,
    chunks: &mut Vec<TextChunk>,
) {
    let mut byte_offset = 0;
    while byte_offset < line.len() {
        let rest = &line[byte_offset..];
        let slice_chars = if utf16_len(rest) <= max_chars {
            utf16_len(rest)
        } else {
            find_line_cut(rest, max_chars)
        };
        let slice_bytes = byte_offset_at_utf16_ceil(rest, slice_chars);
        let slice = &rest[..slice_bytes];
        if !slice.trim().is_empty() {
            chunks.push(TextChunk {
                text: slice.to_owned(),
                range: TextRange {
                    start_line: line_index + 1,
                    end_line: line_index + 1,
                    start_utf16_offset: line_offset + utf16_len(&line[..byte_offset]),
                    end_utf16_offset: line_offset + utf16_len(&line[..byte_offset + slice_bytes]),
                },
            });
        }
        byte_offset += slice_bytes;
    }
}

fn compute_next_start_line(
    lines: &[&str],
    start_index: usize,
    end_index: usize,
    overlap_chars: usize,
) -> usize {
    if overlap_chars == 0 {
        return end_index;
    }

    let mut overlap_lines = 0;
    let mut overlap_count = 0;
    for index in (start_index..end_index).rev() {
        if overlap_count >= overlap_chars {
            break;
        }
        overlap_count += utf16_len(lines[index]) + 1;
        overlap_lines += 1;
    }
    let next_start = end_index - overlap_lines;
    if next_start > start_index {
        next_start
    } else {
        end_index
    }
}

fn find_line_cut(line: &str, max_chars: usize) -> usize {
    if utf16_len(line) <= max_chars {
        return utf16_len(line);
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

#[cfg(test)]
mod tests {
    use crate::domain::{Content, FileFormat, SourceRange, TextRange};

    use super::super::test_content;

    use super::super::{ChunkOptions, test_source};
    use super::extract;
    use crate::utils::byte_offset_at_utf16_floor;

    #[test]
    fn validates_chunks_ranges_and_overlap_like_typescript() {
        let source = test_source(
            FileFormat::Text,
            "fixture.txt",
            "alpha beta\ngamma delta\nepsilon zeta\n",
        );
        let chunks = extract(
            &source,
            ChunkOptions {
                max_chunk_chars: Some(18),
                chunk_overlap_chars: Some(6),
            },
        )
        .expect("text extraction");
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0].document_id().len(), 64);
        assert_eq!(
            *chunks[0].range(),
            SourceRange::Text(TextRange {
                start_line: 1,
                end_line: 1,
                start_utf16_offset: 0,
                end_utf16_offset: 10,
            })
        );
        for chunk in &chunks {
            let Content::Text(text) = &test_content(chunk) else {
                panic!("text fragment expected");
            };
            assert!(text.chars().count() <= 18);
        }
    }

    #[test]
    fn splits_long_unicode_lines_on_character_boundaries() {
        let text = format!("prefix {} suffix", "😀".repeat(20));
        let source = test_source(FileFormat::Text, "unicode.txt", &text);
        let chunks = extract(
            &source,
            ChunkOptions {
                max_chunk_chars: Some(10),
                chunk_overlap_chars: Some(0),
            },
        )
        .expect("unicode extraction");
        assert!(chunks.len() >= 3);
        for chunk in chunks {
            let Content::Text(content) = test_content(&chunk) else {
                panic!("text fragment expected");
            };
            assert!(content.chars().count() <= 10);
            let SourceRange::Text(TextRange {
                start_utf16_offset,
                end_utf16_offset,
                ..
            }) = *chunk.range()
            else {
                panic!("text range expected");
            };
            let start_byte = byte_offset_at_utf16_floor(&source.text, start_utf16_offset);
            let end_byte = byte_offset_at_utf16_floor(&source.text, end_utf16_offset);
            assert_eq!(content, source.text[start_byte..end_byte]);
        }
    }

    #[test]
    fn rejects_invalid_options_and_empty_metadata() {
        let source = test_source(FileFormat::Text, "fixture.txt", "value");
        assert!(
            extract(
                &source,
                ChunkOptions {
                    max_chunk_chars: Some(0),
                    chunk_overlap_chars: None,
                }
            )
            .is_err()
        );
        assert!(
            extract(
                &source,
                ChunkOptions {
                    max_chunk_chars: Some(10),
                    chunk_overlap_chars: Some(10),
                }
            )
            .is_err()
        );

        let blank = test_source(FileFormat::Text, "blank.txt", " \n\t");
        assert!(
            extract(&blank, ChunkOptions::default())
                .expect("blank extraction")
                .is_empty()
        );
    }
}
