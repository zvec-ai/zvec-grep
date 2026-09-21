use crate::{
    EngineError,
    domain::{Content, Range},
    utils::{byte_offset_at_utf16_ceil, byte_offset_at_utf16_floor, line_byte_offsets, utf16_len},
};

use super::{
    ChunkOptions, ExtractedEntity, ExtractedEntityFragment, TextRange, TextSource,
    chunking::find_line_cut, validate_formats,
};

const DEFAULT_TEXT_CHUNK_CHARS: usize = 3_600;
const DEFAULT_TEXT_CHUNK_OVERLAP_CHARS: usize = 540;

pub(super) fn extract(
    source: &TextSource,
    options: ChunkOptions,
) -> Result<Vec<ExtractedEntity>, EngineError> {
    validate_formats(&source.formats)?;
    let (max_chars, overlap_chars) = resolve_options(options)?;
    Ok(extract_plain_text_entities(
        source,
        max_chars,
        overlap_chars,
    ))
}

pub(super) fn extract_plain_text_entities(
    source: &TextSource,
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<ExtractedEntity> {
    chunk_text(&source.text, max_chars, overlap_chars)
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| ExtractedEntity {
            index,
            source_range: Range::Text(chunk.range),
            content: Content::Text(chunk.text),
            metadata: None,
            fragments: vec![ExtractedEntityFragment { range: Range::Full }],
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
    let line_offsets = line_byte_offsets(text);
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
                range: TextRange::from_coordinates(
                    line_offsets[start_index],
                    line_offsets[end_line_index] + lines[end_line_index].len(),
                    start_index + 1,
                    end_index,
                    0,
                    lines[end_line_index].len(),
                )
                .expect("chunk coordinates refer to source lines"),
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
        let slice_chars = find_line_cut(rest, max_chars);
        let mut slice_bytes = byte_offset_at_utf16_floor(rest, slice_chars);
        if slice_bytes == 0 {
            slice_bytes = byte_offset_at_utf16_ceil(rest, slice_chars);
        }
        let slice = &rest[..slice_bytes];
        if !slice.trim().is_empty() {
            chunks.push(TextChunk {
                text: slice.to_owned(),
                range: TextRange::from_coordinates(
                    line_offset + byte_offset,
                    line_offset + byte_offset + slice_bytes,
                    line_index + 1,
                    line_index + 1,
                    byte_offset,
                    byte_offset + slice_bytes,
                )
                .expect("chunk coordinates refer to a source line"),
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

#[cfg(test)]
mod tests {
    use crate::domain::{Content, FileFormat, Range, TextRange};

    use super::super::test_content;

    use super::super::{ChunkOptions, test_source};
    use super::{chunk_text, extract};

    #[test]
    fn unicode_chunks_obey_the_budget_and_advance_at_tiny_limits() {
        for (text, max_chars, expected) in [
            ("aaa😀z", 4, vec!["aaa", "😀z"]),
            ("a😀z", 2, vec!["a", "😀", "z"]),
            ("😀a😀", 1, vec!["😀", "a", "😀"]),
        ] {
            let chunks = chunk_text(text, max_chars, 0);
            assert_eq!(
                chunks
                    .iter()
                    .map(|chunk| chunk.text.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            let mut end = 0;
            for chunk in chunks {
                assert!(
                    crate::utils::utf16_len(&chunk.text) <= max_chars
                        || (max_chars == 1 && chunk.text.chars().count() == 1)
                );
                assert_eq!(chunk.range.start_byte_offset(), end);
                end = chunk.range.end_byte_offset();
                assert_eq!(&text[chunk.range.start_byte_offset()..end], chunk.text);
            }
            assert_eq!(end, text.len());
        }
    }

    #[test]
    fn long_lines_keep_punctuation_cuts_and_exact_source_coverage() {
        let prefix = "aaaaaaa.;-";
        let short = chunk_text(prefix, 10, 0);
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].text, prefix);

        let text = format!("{prefix}{}", "中😀z".repeat(16 * 1024));
        let chunks = chunk_text(&text, 10, 0);
        assert_eq!(chunks[0].text, "aaaaaaa.");
        let mut end = 0;
        for chunk in chunks {
            assert!(crate::utils::utf16_len(&chunk.text) <= 10);
            assert_eq!(chunk.range.start_byte_offset(), end);
            end = chunk.range.end_byte_offset();
            assert_eq!(&text[chunk.range.start_byte_offset()..end], chunk.text);
        }
        assert_eq!(end, text.len());
    }

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
        assert_eq!(chunks[0].index(), 0);
        assert_eq!(
            *chunks[0].source_range(),
            Range::Text(TextRange::from_coordinates(0, 10, 1, 1, 0, 10).expect("first line"))
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
        let text = format!("前言\r\nprefix {} suffix\r\n尾声", "😀".repeat(20));
        let source = test_source(FileFormat::Text, "unicode.txt", &text);
        let line_starts = [0, "前言\r\n".len(), text.len() - "尾声".len()];
        for max_chars in [1, 10] {
            let chunks = extract(
                &source,
                ChunkOptions {
                    max_chunk_chars: Some(max_chars),
                    chunk_overlap_chars: Some(0),
                },
            )
            .expect("unicode extraction");
            assert!(chunks.len() >= 3);
            for chunk in chunks {
                let Content::Text(content) = test_content(&chunk) else {
                    panic!("text fragment expected");
                };
                assert!(content.chars().count() <= max_chars);
                let Range::Text(range) = *chunk.source_range() else {
                    panic!("text range expected");
                };
                assert_eq!(
                    source
                        .text
                        .get(range.start_byte_offset()..range.end_byte_offset()),
                    Some(content.as_str())
                );
                assert_eq!(
                    range.start_byte_offset(),
                    line_starts[range.start_line() - 1] + range.start_byte_column()
                );
                assert_eq!(
                    range.end_byte_offset(),
                    line_starts[range.end_line() - 1] + range.end_byte_column()
                );
            }
        }

        let source = test_source(FileFormat::Text, "whitespace.txt", "\t中😀 \r\n\r\n");
        let chunks = extract(&source, ChunkOptions::default()).expect("whitespace extraction");
        assert_eq!(chunks.len(), 1);
        assert_eq!(test_content(&chunks[0]), Content::Text(source.text.clone()));
        let Range::Text(range) = chunks[0].source_range() else {
            panic!("text range expected");
        };
        assert_eq!(
            crate::utils::slice_text(
                &source.text,
                range.start_byte_offset(),
                range.end_byte_offset()
            )
            .expect("full source span"),
            source.text
        );
        assert_eq!((range.end_line(), range.end_byte_column()), (3, 0));
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
