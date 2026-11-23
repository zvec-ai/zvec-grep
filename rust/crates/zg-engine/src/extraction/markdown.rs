use crate::{
    EngineError,
    domain::{Content, EntityMetadata, FileFormat, MarkdownMetadata, Range},
};

use super::{
    ChunkOptions, ExtractedEntity, TextSource, chunk_options_for_metadata,
    chunking::text_fragments,
    sectioning::{Heading, Section, split_sections},
    source_map::{SourceMap, TextSourceMap},
    text::extract_plain_text_entities,
    validate_formats,
};

const DEFAULT_MARKDOWN_CHUNK_CHARS: usize = 3_600;
const DEFAULT_MARKDOWN_CHUNK_OVERLAP_CHARS: usize = 540;

pub(super) fn extract(
    source: &TextSource,
    options: ChunkOptions,
) -> Result<Vec<ExtractedEntity>, EngineError> {
    if !source.formats.contains(&FileFormat::Markdown) {
        return Ok(Vec::new());
    }
    validate_formats(&source.formats)?;
    let (max_chars, overlap_chars) = resolve_options(options)?;
    let lines = source.text.split('\n').collect::<Vec<_>>();
    let source_map = TextSourceMap::new(&source.text);
    let headings = scan_headings(&lines, source_map.line_starts());
    if headings.is_empty() {
        return Ok(extract_plain_text_entities(
            source,
            max_chars,
            overlap_chars,
        ));
    }
    let entities = split_sections(&source.text, &headings)?
        .into_iter()
        .enumerate()
        .map(|(index, section)| {
            let metadata = markdown_metadata(&section);
            let (content_max, content_overlap) =
                chunk_options_for_metadata(max_chars, overlap_chars, Some(&metadata));
            let range = source_map
                .map_range(section.range)
                .expect("section coordinates refer to source text");
            let content = crate::utils::slice_text(
                &source.text,
                range.start_byte_offset(),
                range.end_byte_offset(),
            )
            .expect("section range refers to source")
            .to_owned();
            ExtractedEntity {
                index,
                source_range: Range::Text(range),
                fragments: text_fragments(&content, content_max, content_overlap),
                content: Content::Text(content),
                metadata: Some(metadata),
            }
        })
        .collect::<Vec<_>>();
    if entities.is_empty() {
        Ok(extract_plain_text_entities(
            source,
            max_chars,
            overlap_chars,
        ))
    } else {
        Ok(entities)
    }
}

fn resolve_options(options: ChunkOptions) -> Result<(usize, usize), EngineError> {
    let max_chars = options
        .max_chunk_chars
        .unwrap_or(DEFAULT_MARKDOWN_CHUNK_CHARS);
    let overlap_chars = options
        .chunk_overlap_chars
        .unwrap_or(DEFAULT_MARKDOWN_CHUNK_OVERLAP_CHARS);
    if max_chars == 0 {
        return Err(EngineError::invalid_argument(
            "markdown extractor requires a positive integer chunk size",
        ));
    }
    if overlap_chars >= max_chars {
        return Err(EngineError::invalid_argument(
            "markdown extractor requires overlap to be smaller than chunk size",
        ));
    }
    Ok((max_chars, overlap_chars))
}

fn scan_headings(lines: &[&str], line_offsets: &[usize]) -> Vec<Heading> {
    let mut headings = Vec::new();
    let mut fence = None;
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
            }
            index += 1;
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
            index += 1;
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~");
            index += 1;
            continue;
        }

        if let Some((level, text)) = parse_atx_heading(line) {
            headings.push(Heading {
                level,
                title: text,
                start: line_offsets[index],
                previous_end: preceding_section_end(line_offsets[index]),
            });
            index += 1;
            continue;
        }

        if !line.trim().is_empty() && index + 1 < lines.len() {
            let next = lines[index + 1].trim();
            if !next.is_empty() && next.chars().all(|character| character == '=') {
                headings.push(Heading {
                    level: 1,
                    title: line.trim().to_owned(),
                    start: line_offsets[index],
                    previous_end: preceding_section_end(line_offsets[index]),
                });
                index += 2;
                continue;
            }
            if !next.is_empty() && next.chars().all(|character| character == '-') {
                headings.push(Heading {
                    level: 2,
                    title: line.trim().to_owned(),
                    start: line_offsets[index],
                    previous_end: preceding_section_end(line_offsets[index]),
                });
                index += 2;
                continue;
            }
        }
        index += 1;
    }
    headings
}

fn parse_atx_heading(line: &str) -> Option<(usize, String)> {
    if !line.starts_with('#') {
        return None;
    }
    let level = line.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim_end().to_owned();
    (!text.is_empty()).then_some((level, text))
}

fn preceding_section_end(heading_start: usize) -> usize {
    // Preserve the existing Markdown contract: omit only the LF before a
    // heading, retaining any CR and all other whitespace in the prior section.
    heading_start.saturating_sub(1)
}

fn markdown_metadata(section: &Section<'_>) -> EntityMetadata {
    EntityMetadata::Markdown(MarkdownMetadata {
        heading: section
            .heading
            .as_ref()
            .map(|heading| heading.title.clone()),
        level: section.heading.as_ref().map(|heading| heading.level),
        scope: (!section.ancestors.is_empty()).then(|| section.ancestors.join("::")),
    })
}

#[cfg(test)]
mod tests {
    use crate::domain::{Content, EntityMetadata, FileFormat, MarkdownMetadata, Range, TextRange};

    use super::super::{test_content, test_metadata};

    use super::super::{ChunkOptions, test_source};
    use super::extract;

    fn extraction_snapshot(
        entities: &[crate::extraction::ExtractedEntity],
        maximum: usize,
    ) -> Vec<serde_json::Value> {
        entities
            .iter()
            .map(|entity| {
                let id = crate::domain::EntityId::new(
                    crate::domain::FileId::new(42),
                    &entity.content,
                    &entity.source_range,
                )
                .expect("entity ID");
                let Content::Text(text) = &entity.content else {
                    panic!("text entity expected");
                };
                let fragments = entity
                    .fragments
                    .iter()
                    .map(|fragment| {
                        let content = match fragment.range {
                            Range::Full => text.clone(),
                            Range::Byte(range) => crate::utils::slice_text(
                                text,
                                usize::try_from(range.start_offset()).expect("start"),
                                usize::try_from(range.end_offset()).expect("end"),
                            )
                            .expect("fragment text")
                            .to_owned(),
                            Range::Text(_) | Range::Pdf(_) => {
                                panic!("entity-relative range expected")
                            }
                        };
                        serde_json::json!({
                            "range": fragment.range,
                            "embedding": crate::extraction::vector_content_for_fragment(
                                &Content::Text(content), entity.metadata.as_ref(), Some(maximum)
                            ),
                        })
                    })
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "index": entity.index,
                    "id": id.as_str(),
                    "range": entity.source_range,
                    "content": entity.content,
                    "metadata": entity.metadata,
                    "fragments": fragments,
                })
            })
            .collect()
    }

    #[test]
    fn preserves_markdown_extraction_snapshots() {
        // These snapshots cover the externally observable extraction contract,
        // including exact whitespace, coordinates, IDs, and embedding inputs.
        let fixtures = [
            "前言 😀\n\n# Parent #\nintro\n## Child\nchild 😀\n#### Deep\nnested\n## Child\nrepeat\nSibling\n=======\nbody\n",
            "# Parent\n## Child\nChild two\n---------\nbody",
            "  \n\n# Title\n\n```md\n# Hidden\n```\n~~~\n## Hidden\n~~~\n## Visible\nlast\n",
            "plain text 😀 without headings\n\nA long line that must be split into multiple retrieval chunks without losing its original source coordinates.\n",
        ];
        let mut snapshots = Vec::new();
        for fixture in fixtures {
            for newline in ["\n", "\r\n"] {
                let source = test_source(
                    FileFormat::Markdown,
                    "README.md",
                    &fixture.replace('\n', newline),
                );
                let mut outputs = Vec::new();
                for (maximum, overlap) in [(64, 8), (3_600, 540)] {
                    let options = ChunkOptions {
                        max_chunk_chars: Some(maximum),
                        chunk_overlap_chars: Some(overlap),
                    };
                    let entities = extract(&source, options).expect("markdown extraction");
                    assert_eq!(
                        entities,
                        crate::extraction::extract(&source, options).expect("search extraction")
                    );
                    for output in [
                        crate::extraction::extract_for_indexing(&source, options),
                        crate::extraction::extract_without_graph(&source, options),
                    ] {
                        let output = output.expect("indexing extraction");
                        assert_eq!(entities, output.fragments);
                        assert!(output.graph.is_none());
                    }
                    outputs.push(extraction_snapshot(&entities, maximum));
                }
                // Workspace builds enable serde_json's preserve_order through
                // the CLI. Canonicalize objects so feature unification cannot
                // change snapshots captured by an engine-only test build.
                let mut snapshot = serde_json::to_value(outputs).expect("extraction snapshot");
                snapshot.sort_all_objects();
                snapshots.push(crate::utils::sha256_hex(
                    &serde_json::to_vec(&snapshot).expect("serialize extraction snapshot"),
                ));
            }
        }
        // Captured from the original Markdown extractor before sharing sectioning.
        // Each fixture contributes LF and CRLF snapshots, in that order.
        assert_eq!(
            snapshots,
            [
                "ec2046eeaa03c9cc02586968731535d3885467ae405608dcda1517ca3c82b2b8",
                "79561b1ef4aa9988ee9a9a87c8a12a9269995989b1112d7b057dd73a11601a5d",
                "ea77d848d828a4343f62e2fa4e82ddbbc4ecd0f01c554977ce40ae10dbb07e55",
                "c1bafcf851120886910a928101260ff72e709bc76622d5c692d5f1809d370b7c",
                "f05e7f56903b0fa704e62b2208e911a993aa51708b004a510f128e096c657a03",
                "f843e792d43671f923aaf76908525c53f837b9c804e0e43cb3882233b6eafd30",
                "e56c797be8d9d028c8111c8b82b864c5563034325c1663d839a1210d601c2bfb",
                "6e9b2e22aedf49f936c2411281a5a88aedfc5a28b9da2d114feb8a4cc2a30d07",
            ]
        );
    }

    #[test]
    fn handles_heading_styles_fences_hierarchy_and_windows() {
        let source = test_source(
            FileFormat::Markdown,
            "README.md",
            &[
                "前言 😀",
                "",
                "# Parent #",
                "intro paragraph",
                "```md",
                "# Not a heading",
                "```",
                "## Child",
                "- item one",
                "- 项目 😀 with enough text to force another window",
                "Setext child",
                "------------",
                "body",
                "",
            ]
            .join("\r\n"),
        );
        let fragments = extract(
            &source,
            ChunkOptions {
                max_chunk_chars: Some(48),
                chunk_overlap_chars: Some(8),
            },
        )
        .expect("markdown extraction");
        assert!(fragments.len() >= 4);
        assert!(fragments.iter().any(|item| matches!(
            &test_metadata(item),
            Some(EntityMetadata::Markdown(MarkdownMetadata { heading: Some(heading), .. })) if heading == "Parent"
        )));
        assert!(fragments.iter().any(|item| matches!(
            &test_metadata(item),
            Some(EntityMetadata::Markdown(MarkdownMetadata {
                heading: Some(heading),
                scope: Some(scope),
                ..
            })) if heading == "Child" && scope == "Parent"
        )));
        assert!(!fragments.iter().any(|item| matches!(
            &test_metadata(item),
            Some(EntityMetadata::Markdown(MarkdownMetadata { heading: Some(heading), .. })) if heading == "Not a heading"
        )));
        assert!(fragments.iter().any(|entity| entity.fragments.len() > 1));
        assert_eq!(
            *fragments[0].source_range(),
            Range::Text(
                TextRange::from_coordinates(0, "前言 😀\r\n\r".len(), 1, 2, 0, 1)
                    .expect("preamble coordinates")
            )
        );

        assert_source_backed(&source, &fragments);
    }

    fn assert_source_backed(source: &super::TextSource, fragments: &[super::ExtractedEntity]) {
        for (index, entity) in fragments.iter().enumerate() {
            assert_eq!(entity.index, index);
            let Content::Text(content) = &entity.content else {
                panic!("text entity expected");
            };
            let Range::Text(range) = entity.source_range else {
                panic!("text range expected");
            };
            assert_eq!(
                crate::utils::slice_text(
                    &source.text,
                    range.start_byte_offset(),
                    range.end_byte_offset()
                )
                .expect("source range"),
                content
            );
            let mut covered = vec![false; content.len()];
            for fragment in &entity.fragments {
                let (start, end) = match fragment.range {
                    Range::Full => (0, content.len()),
                    Range::Byte(local) => (
                        usize::try_from(local.start_offset()).expect("fragment start"),
                        usize::try_from(local.end_offset()).expect("fragment end"),
                    ),
                    Range::Text(_) | Range::Pdf(_) => panic!("fragments must store byte offsets"),
                };
                assert!(start < end && end <= content.len());
                assert!(content.is_char_boundary(start) && content.is_char_boundary(end));
                covered[start..end].fill(true);
            }
            assert!(
                content.char_indices().all(|(offset, character)| {
                    character.is_whitespace()
                        || covered[offset..offset + character.len_utf8()]
                            .iter()
                            .all(|value| *value)
                }),
                "every non-whitespace source byte belongs to a fragment"
            );
        }
    }

    #[test]
    fn falls_back_without_headings_and_validates_options() {
        let source = test_source(FileFormat::Markdown, "README.md", "plain markdown");
        let fragments = extract(&source, ChunkOptions::default()).expect("fallback");
        assert_eq!(fragments.len(), 1);
        assert_eq!(
            test_content(&fragments[0]),
            Content::Text("plain markdown".to_owned())
        );
        assert!(test_metadata(&fragments[0]).is_none());

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
    }

    #[test]
    fn ignores_non_markdown_sources() {
        let source = test_source(FileFormat::Text, "README.txt", "# Heading");
        assert!(
            extract(&source, ChunkOptions::default())
                .expect("non-markdown")
                .is_empty()
        );
    }
}
