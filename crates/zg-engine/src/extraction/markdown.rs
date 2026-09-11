use crate::{
    EngineError,
    domain::{
        Content, Entity, EntityContent, EntityId, EntityMetadata, FileFormat, FragmentId,
        SourceRange, WindowFragment,
    },
};

use super::{
    ChunkOptions, EntityFragment, TextRange, TextSource, byte_index_at_utf16_ceil, char_count,
    chunk_options_for_metadata, fit_text_to_chars, make_entity_id,
    text::extract_plain_text_fragments, validate_source_file,
};

const DEFAULT_MARKDOWN_CHUNK_CHARS: usize = 3_600;
const DEFAULT_MARKDOWN_CHUNK_OVERLAP_CHARS: usize = 540;

#[derive(Clone, Debug)]
struct Heading {
    level: usize,
    text: String,
    line_index: usize,
}

#[derive(Clone, Debug)]
struct Section {
    heading: Option<Heading>,
    start_index: usize,
    end_index: usize,
    breadcrumb: Vec<String>,
}

#[derive(Debug)]
struct MarkdownWindow {
    text: String,
    range: TextRange,
}

pub(super) fn extract(
    source: &TextSource,
    options: ChunkOptions,
) -> Result<Vec<EntityFragment>, EngineError> {
    if !source.file.formats.contains(&FileFormat::Markdown) {
        return Ok(Vec::new());
    }
    validate_source_file(&source.file)?;
    let (max_chars, overlap_chars) = resolve_options(options)?;
    let lines = source.text.split('\n').collect::<Vec<_>>();
    let headings = scan_headings(&lines);
    if headings.is_empty() {
        return Ok(extract_plain_text_fragments(
            source,
            max_chars,
            overlap_chars,
        ));
    }

    let line_offsets = compute_line_offsets(&lines);
    let fence_lines = compute_fence_lines(&lines);
    let sections = build_sections(&headings, &lines);
    let mut fragments = Vec::new();

    for section in sections {
        let metadata = markdown_metadata(&section);
        let (content_max, content_overlap) =
            chunk_options_for_metadata(max_chars, overlap_chars, Some(&metadata));
        let windows = split_markdown_section(
            &lines,
            &line_offsets,
            &fence_lines,
            &section,
            content_max,
            content_overlap,
        );

        if windows.len() > 1 {
            let id = make_entity_id(&source.file.id, fragments.len());
            let section_window = lines_to_window(
                &lines,
                &line_offsets,
                section.start_index,
                section.end_index,
            );
            fragments.push(EntityFragment::Representative(Entity {
                id: id.clone(),
                file_id: source.file.id.clone(),
                range: SourceRange::Text(section_window.range),
                content: EntityContent::Outline(fit_text_to_chars(
                    metadata_heading(&metadata).unwrap_or("markdown section"),
                    content_max,
                )),
                metadata: Some(metadata.clone()),
            }));

            for window in windows {
                let index = fragments.len();
                fragments.push(markdown_window_to_fragment(
                    source,
                    metadata.clone(),
                    window,
                    index,
                    Some(id.clone()),
                ));
            }
        } else {
            for window in windows {
                let index = fragments.len();
                fragments.push(markdown_window_to_fragment(
                    source,
                    metadata.clone(),
                    window,
                    index,
                    None,
                ));
            }
        }
    }

    if fragments.is_empty() {
        Ok(extract_plain_text_fragments(
            source,
            max_chars,
            overlap_chars,
        ))
    } else {
        Ok(fragments)
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

fn markdown_window_to_fragment(
    source: &TextSource,
    metadata: EntityMetadata,
    window: MarkdownWindow,
    index: usize,
    owner: Option<EntityId>,
) -> EntityFragment {
    let id = make_entity_id(&source.file.id, index);
    let range = SourceRange::Text(window.range);
    let contents = vec![Content::Text(window.text)];
    match owner {
        Some(entity_id) => EntityFragment::Window(WindowFragment {
            id: FragmentId::new(id.as_str()).expect("generated fragment id"),
            entity_id,
            file_id: source.file.id.clone(),
            range,
            contents,
            metadata: Some(metadata),
        }),
        None => EntityFragment::Standalone(Entity {
            id,
            file_id: source.file.id.clone(),
            range,
            content: EntityContent::Source(contents),
            metadata: Some(metadata),
        }),
    }
}

fn scan_headings(lines: &[&str]) -> Vec<Heading> {
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
                text,
                line_index: index,
            });
            index += 1;
            continue;
        }

        if !line.trim().is_empty() && index + 1 < lines.len() {
            let next = lines[index + 1].trim();
            if !next.is_empty() && next.chars().all(|character| character == '=') {
                headings.push(Heading {
                    level: 1,
                    text: line.trim().to_owned(),
                    line_index: index,
                });
                index += 2;
                continue;
            }
            if !next.is_empty() && next.chars().all(|character| character == '-') {
                headings.push(Heading {
                    level: 2,
                    text: line.trim().to_owned(),
                    line_index: index,
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

fn build_sections(headings: &[Heading], lines: &[&str]) -> Vec<Section> {
    let mut stack: Vec<&Heading> = Vec::new();
    let mut sections = Vec::new();
    let first = &headings[0];
    if first.line_index > 0 && !lines[..first.line_index].join("\n").trim().is_empty() {
        sections.push(Section {
            heading: None,
            start_index: 0,
            end_index: first.line_index - 1,
            breadcrumb: Vec::new(),
        });
    }

    for (index, heading) in headings.iter().enumerate() {
        while stack.last().is_some_and(|item| item.level >= heading.level) {
            stack.pop();
        }
        sections.push(Section {
            heading: Some(heading.clone()),
            start_index: heading.line_index,
            end_index: headings
                .get(index + 1)
                .map_or(lines.len() - 1, |next| next.line_index - 1),
            breadcrumb: stack.iter().map(|item| item.text.clone()).collect(),
        });
        stack.push(heading);
    }
    sections
}

fn split_markdown_section(
    lines: &[&str],
    line_offsets: &[usize],
    fence_lines: &[bool],
    section: &Section,
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<MarkdownWindow> {
    let mut windows = Vec::new();
    let mut start_index = section.start_index;

    while start_index <= section.end_index {
        if char_count(lines[start_index]) + 1 > max_chars {
            windows.extend(split_long_line(
                lines[start_index],
                start_index,
                line_offsets[start_index],
                max_chars,
            ));
            start_index += 1;
            continue;
        }

        let mut end_index = start_index;
        let mut used_chars = 0;
        while end_index <= section.end_index {
            let line_length = char_count(lines[end_index]) + 1;
            if used_chars + line_length > max_chars && end_index > start_index {
                break;
            }
            used_chars += line_length;
            end_index += 1;
        }

        if end_index <= section.end_index && end_index - start_index > 1 {
            end_index = choose_markdown_break(lines, fence_lines, start_index, end_index);
        }
        windows.push(lines_to_window(
            lines,
            line_offsets,
            start_index,
            end_index - 1,
        ));
        if end_index > section.end_index {
            break;
        }

        let overlap_lines =
            compute_markdown_overlap_lines(lines, start_index, end_index, overlap_chars);
        let next_start = end_index - overlap_lines;
        start_index = if next_start > start_index {
            next_start
        } else {
            end_index
        };
    }

    windows
        .into_iter()
        .filter(|window| !window.text.trim().is_empty())
        .collect()
}

fn choose_markdown_break(
    lines: &[&str],
    fence_lines: &[bool],
    start_index: usize,
    end_index: usize,
) -> usize {
    let min_break = start_index + ((end_index - start_index) * 7 / 10).max(1);
    let mut best_break = end_index;
    let mut best_score = markdown_break_score(lines, fence_lines, end_index);
    for index in min_break..=end_index {
        let score = markdown_break_score(lines, fence_lines, index);
        if score > best_score {
            best_break = index;
            best_score = score;
        }
    }
    best_break
}

fn markdown_break_score(lines: &[&str], fence_lines: &[bool], index: usize) -> usize {
    if index == 0 || index >= lines.len() || fence_lines[index] {
        return 0;
    }
    let current = lines[index].trim();
    let previous = lines[index - 1].trim();
    if parse_atx_heading(current).is_some() {
        100
    } else if previous.is_empty() && current.is_empty() {
        70
    } else if previous.is_empty() {
        60
    } else if is_list_item(current) {
        35
    } else if current.starts_with("> ") {
        25
    } else {
        10
    }
}

fn is_list_item(line: &str) -> bool {
    if line.starts_with("- ") || line.starts_with("* ") || line.starts_with("+ ") {
        return true;
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && line[digits..].starts_with(". ")
}

fn split_long_line(
    line: &str,
    line_index: usize,
    line_offset: usize,
    max_chars: usize,
) -> Vec<MarkdownWindow> {
    let mut windows = Vec::new();
    let mut byte_offset = 0;
    while byte_offset < line.len() {
        let rest = &line[byte_offset..];
        let slice_chars = if char_count(rest) <= max_chars {
            char_count(rest)
        } else {
            find_line_cut(rest, max_chars)
        };
        let slice_bytes = byte_index_at_utf16_ceil(rest, slice_chars);
        let text = &rest[..slice_bytes];
        windows.push(MarkdownWindow {
            text: text.to_owned(),
            range: TextRange {
                start_line: line_index + 1,
                end_line: line_index + 1,
                start_utf16_offset: line_offset + char_count(&line[..byte_offset]),
                end_utf16_offset: line_offset + char_count(&line[..byte_offset + slice_bytes]),
            },
        });
        byte_offset += slice_bytes;
    }
    windows
}

fn lines_to_window(
    lines: &[&str],
    line_offsets: &[usize],
    start_index: usize,
    end_index: usize,
) -> MarkdownWindow {
    MarkdownWindow {
        text: lines[start_index..=end_index].join("\n"),
        range: TextRange {
            start_line: start_index + 1,
            end_line: end_index + 1,
            start_utf16_offset: line_offsets[start_index],
            end_utf16_offset: line_offsets[end_index] + char_count(lines[end_index]),
        },
    }
}

fn compute_line_offsets(lines: &[&str]) -> Vec<usize> {
    let mut offset = 0;
    lines
        .iter()
        .map(|line| {
            let current = offset;
            offset += char_count(line) + 1;
            current
        })
        .collect()
}

fn compute_fence_lines(lines: &[&str]) -> Vec<bool> {
    let mut in_fence = vec![false; lines.len()];
    let mut fence = None;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if let Some(marker) = fence {
            in_fence[index] = true;
            if trimmed.starts_with(marker) {
                fence = None;
            }
        } else if trimmed.starts_with("```") {
            fence = Some("```");
        } else if trimmed.starts_with("~~~") {
            fence = Some("~~~");
        }
    }
    in_fence
}

fn compute_markdown_overlap_lines(
    lines: &[&str],
    start_index: usize,
    end_index: usize,
    overlap_chars: usize,
) -> usize {
    if overlap_chars == 0 {
        return 0;
    }
    let mut chars = 0;
    let mut count = 0;
    for index in ((start_index + 1)..end_index).rev() {
        chars += char_count(lines[index]) + 1;
        if chars > overlap_chars {
            break;
        }
        count += 1;
    }
    count.min((end_index - start_index) / 2)
}

fn find_line_cut(line: &str, max_chars: usize) -> usize {
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

fn markdown_metadata(section: &Section) -> EntityMetadata {
    EntityMetadata::Markdown {
        heading: section.heading.as_ref().map(|heading| heading.text.clone()),
        level: section.heading.as_ref().map(|heading| heading.level),
        scope: (!section.breadcrumb.is_empty()).then(|| section.breadcrumb.join("::")),
    }
}

fn metadata_heading(metadata: &EntityMetadata) -> Option<&str> {
    match metadata {
        EntityMetadata::Markdown { heading, .. } => heading.as_deref(),
        EntityMetadata::Code { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::{
        Content, EntityFragment, EntityMetadata, FileFormat, SourceRange, TextRange,
    };

    use super::super::test_content;

    use super::super::{ChunkOptions, byte_index_at_utf16, test_source};
    use super::extract;

    #[test]
    fn handles_heading_styles_fences_hierarchy_and_windows() {
        let source = test_source(
            FileFormat::Markdown,
            "README.md",
            &[
                "preface 😀",
                "",
                "# Parent #",
                "intro paragraph",
                "```md",
                "# Not a heading",
                "```",
                "## Child",
                "- item one",
                "- item two with enough text to force another window",
                "Setext child",
                "------------",
                "body",
            ]
            .join("\n"),
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
            &item.metadata(),
            Some(EntityMetadata::Markdown { heading: Some(heading), .. }) if heading == "Parent"
        )));
        assert!(fragments.iter().any(|item| matches!(
            &item.metadata(),
            Some(EntityMetadata::Markdown {
                heading: Some(heading),
                scope: Some(scope),
                ..
            }) if heading == "Child" && scope == "Parent"
        )));
        assert!(!fragments.iter().any(|item| matches!(
            &item.metadata(),
            Some(EntityMetadata::Markdown { heading: Some(heading), .. }) if heading == "Not a heading"
        )));
        assert!(fragments.iter().any(|item| matches!(
            item,
            EntityFragment::Representative(_) | EntityFragment::Window(_)
        )));

        for fragment in fragments {
            if matches!(fragment, EntityFragment::Representative(_)) {
                continue;
            }
            let Content::Text(content) = test_content(&fragment) else {
                panic!("text content expected");
            };
            let SourceRange::Text(TextRange {
                start_utf16_offset,
                end_utf16_offset,
                ..
            }) = *fragment.range()
            else {
                panic!("text range expected");
            };
            let start_byte = byte_index_at_utf16(&source.text, start_utf16_offset);
            let end_byte = byte_index_at_utf16(&source.text, end_utf16_offset);
            assert_eq!(content, source.text[start_byte..end_byte]);
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
        assert!(fragments[0].metadata().is_none());

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
