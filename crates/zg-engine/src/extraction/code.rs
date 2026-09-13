mod adapter;

use std::collections::{HashMap, HashSet};

use tree_sitter::{Language, Node, Parser};

use crate::{
    EngineError,
    domain::{
        Content, Entity, EntityContent, EntityFragment, EntityMetadata, FileFormat, FileId,
        FragmentId, SourceRange, SymbolType, WindowFragment,
    },
    utils::{
        byte_offset_at_utf16_ceil, byte_offset_at_utf16_floor, collapse_whitespace,
        line_byte_offsets, take_utf16, utf16_len,
    },
};

use self::adapter::{LanguageAdapter, named_children, resolve_adapter, text};
use super::{
    ChunkOptions, IndexingExtractionFragment, TextRange, TextSource, chunk_options_for_metadata,
    make_entity_id, symbol_type_name, text::extract_plain_text_fragments, validate_source_file,
};

const DEFAULT_CODE_CHUNK_CHARS: usize = 3_600;
const DEFAULT_CODE_CHUNK_OVERLAP_CHARS: usize = 540;
const COMPONENT_CODE_FORMATS: [FileFormat; 2] = [FileFormat::Vue, FileFormat::Svelte];
const OUTLINE_MAX_MEMBERS: usize = 32;
const OUTLINE_MAX_CALLS: usize = 24;
const OUTLINE_MAX_LINE_CHARS: usize = 180;

pub(super) fn extract_for_indexing(
    source: &TextSource,
    options: ChunkOptions,
) -> Result<Vec<IndexingExtractionFragment>, EngineError> {
    let jsx = source
        .file
        .absolute_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("tsx"));
    extract_code(source, options, jsx)
}

fn extract_code(
    source: &TextSource,
    options: ChunkOptions,
    jsx: bool,
) -> Result<Vec<IndexingExtractionFragment>, EngineError> {
    if !super::service::is_code_source(&source.file) {
        return Ok(Vec::new());
    }
    validate_source_file(&source.file)?;
    let (max_chars, overlap_chars) = resolve_options(options)?;

    if source
        .file
        .formats
        .iter()
        .any(|format| COMPONENT_CODE_FORMATS.contains(format))
    {
        let fragments = extract_script_blocks(source, max_chars, overlap_chars)?;
        return if fragments.is_empty() {
            Ok(fallback(source, max_chars, overlap_chars))
        } else {
            Ok(fragments)
        };
    }

    let format = if source.file.formats.contains(&FileFormat::Cpp) {
        Some(FileFormat::Cpp)
    } else {
        source
            .file
            .formats
            .iter()
            .copied()
            .find(|format| resolve_adapter(*format).is_some())
    };
    let Some((adapter, language)) =
        format.and_then(|format| Some((resolve_adapter(format)?, grammar(format, jsx)?)))
    else {
        return Ok(fallback(source, max_chars, overlap_chars));
    };

    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Ok(fallback(source, max_chars, overlap_chars));
    }
    let Some(tree) = parser.parse(&source.text, None) else {
        return Ok(fallback(source, max_chars, overlap_chars));
    };
    let bytes = source.text.as_bytes();
    let mut entities = Vec::new();
    walk_code_node(tree.root_node(), adapter, bytes, &[], &mut entities);

    let mut output = Vec::new();
    for entity in entities {
        append_entity(
            source,
            adapter,
            &entity,
            max_chars,
            overlap_chars,
            &mut output,
        );
    }
    if output.is_empty() {
        Ok(fallback(source, max_chars, overlap_chars))
    } else {
        Ok(output)
    }
}

fn grammar(format: FileFormat, jsx: bool) -> Option<Language> {
    Some(match format {
        FileFormat::C => tree_sitter_c::LANGUAGE.into(),
        FileFormat::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        FileFormat::Go => tree_sitter_go::LANGUAGE.into(),
        FileFormat::Java => tree_sitter_java::LANGUAGE.into(),
        FileFormat::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        FileFormat::Python => tree_sitter_python::LANGUAGE.into(),
        FileFormat::Rust => tree_sitter_rust::LANGUAGE.into(),
        FileFormat::TypeScript if !jsx => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        FileFormat::TypeScript => tree_sitter_typescript::LANGUAGE_TSX.into(),
        _ => return None,
    })
}

fn resolve_options(options: ChunkOptions) -> Result<(usize, usize), EngineError> {
    let max_chars = options.max_chunk_chars.unwrap_or(DEFAULT_CODE_CHUNK_CHARS);
    let overlap_chars = options
        .chunk_overlap_chars
        .unwrap_or(DEFAULT_CODE_CHUNK_OVERLAP_CHARS);
    if max_chars == 0 {
        return Err(EngineError::invalid_argument(
            "code extractor requires a positive integer chunk size",
        ));
    }
    if overlap_chars >= max_chars {
        return Err(EngineError::invalid_argument(
            "code extractor requires overlap to be smaller than chunk size",
        ));
    }
    Ok((max_chars, overlap_chars))
}

fn fallback(
    source: &TextSource,
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<IndexingExtractionFragment> {
    extract_plain_text_fragments(source, max_chars, overlap_chars)
        .into_iter()
        .map(|fragment| IndexingExtractionFragment {
            fragment,
            embedding_source: None,
        })
        .collect()
}

#[derive(Debug)]
struct CodeEntity<'tree> {
    node: Node<'tree>,
    name: Option<String>,
    symbol_type: SymbolType,
    breadcrumb: Vec<String>,
    signature: Option<String>,
    documentation: Option<String>,
    modifiers: Vec<String>,
}

#[derive(Debug)]
struct CodeWindow {
    text: String,
    embedding_text: Option<String>,
    range: TextRange,
}

#[derive(Debug)]
struct CodeFragmentOutput {
    starts_group: bool,
    range: SourceRange,
    content: Content,
    metadata: EntityMetadata,
    embedding_text: Option<String>,
}

fn walk_code_node<'tree>(
    node: Node<'tree>,
    adapter: &LanguageAdapter,
    source: &[u8],
    breadcrumb: &[String],
    out: &mut Vec<CodeEntity<'tree>>,
) {
    for child in named_children(node) {
        let is_scope = adapter.is_scope(child);
        let is_entity = adapter.is_entity(child);
        if is_entity {
            for entity in adapter.resolve_entities(child, source) {
                let name = adapter.extract_name(entity, source);
                let entity_breadcrumb = adapter.scope_breadcrumb(entity, source, breadcrumb);
                out.push(CodeEntity {
                    node: entity,
                    name,
                    symbol_type: adapter.classify(entity, source, &entity_breadcrumb),
                    breadcrumb: entity_breadcrumb,
                    signature: adapter.extract_signature(entity, source),
                    documentation: LanguageAdapter::extract_doc(entity, source),
                    modifiers: adapter.extract_modifiers(entity, source),
                });
            }
        }

        if is_scope {
            let name = adapter.extract_name(child, source);
            let mut child_breadcrumb = breadcrumb.to_vec();
            if let Some(name) = name {
                child_breadcrumb.push(name);
            }
            walk_code_node(
                adapter.enter_scope_node(child),
                adapter,
                source,
                &child_breadcrumb,
                out,
            );
        } else if !is_entity {
            walk_code_node(child, adapter, source, breadcrumb, out);
        }
    }
}

fn append_entity(
    source: &TextSource,
    adapter: &LanguageAdapter,
    entity: &CodeEntity<'_>,
    max_chars: usize,
    overlap_chars: usize,
    output: &mut Vec<IndexingExtractionFragment>,
) {
    let fragments =
        code_entity_to_search_fragments(source, adapter, entity, max_chars, overlap_chars);
    let major_id = fragments
        .first()
        .filter(|fragment| fragment.starts_group)
        .map(|_| make_entity_id(&source.file.id, output.len()));

    for fragment in fragments {
        let id = make_entity_id(&source.file.id, output.len());
        let entity_fragment = if fragment.starts_group {
            let Content::Text(outline) = fragment.content else {
                unreachable!("code outline is text");
            };
            EntityFragment::Representative(Entity {
                id,
                file_id: source.file.id.clone(),
                range: fragment.range,
                content: EntityContent::Outline(outline),
                metadata: Some(fragment.metadata),
            })
        } else if let Some(entity_id) = &major_id {
            EntityFragment::Window(WindowFragment {
                id: FragmentId::new(id.as_str()).expect("generated fragment id"),
                entity_id: entity_id.clone(),
                file_id: source.file.id.clone(),
                range: fragment.range,
                contents: vec![fragment.content],
                metadata: Some(fragment.metadata),
            })
        } else {
            EntityFragment::Standalone(Entity {
                id,
                file_id: source.file.id.clone(),
                range: fragment.range,
                content: EntityContent::Source(vec![fragment.content]),
                metadata: Some(fragment.metadata),
            })
        };
        output.push(IndexingExtractionFragment {
            embedding_source: fragment
                .embedding_text
                .map(|text| vec![Content::Text(text)]),
            fragment: entity_fragment,
        });
    }
}

fn code_entity_to_search_fragments(
    source: &TextSource,
    adapter: &LanguageAdapter,
    entity: &CodeEntity<'_>,
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<CodeFragmentOutput> {
    let metadata = code_entity_metadata(entity);
    let (content_max, content_overlap) =
        chunk_options_for_metadata(max_chars, overlap_chars, Some(&metadata));
    let node_text = text(entity.node, source.text.as_bytes());
    if utf16_len(node_text) <= content_max {
        return vec![window_to_fragment(
            entity,
            node_to_window(entity.node, source.text.as_bytes()),
        )];
    }

    let major = CodeFragmentOutput {
        starts_group: true,
        range: SourceRange::Text(node_to_window(entity.node, source.text.as_bytes()).range),
        content: Content::Text(code_entity_outline(
            entity,
            adapter,
            source.text.as_bytes(),
            content_max,
        )),
        metadata: metadata.clone(),
        embedding_text: None,
    };
    let mut fragments = vec![major];
    fragments.extend(
        split_large_node(
            entity.node,
            source.text.as_bytes(),
            content_max,
            content_overlap,
        )
        .into_iter()
        .map(|window| window_to_fragment(entity, window)),
    );
    fragments
}

fn window_to_fragment(entity: &CodeEntity<'_>, window: CodeWindow) -> CodeFragmentOutput {
    CodeFragmentOutput {
        starts_group: false,
        range: SourceRange::Text(window.range),
        content: Content::Text(window.text),
        metadata: code_entity_metadata(entity),
        embedding_text: window.embedding_text,
    }
}

fn node_to_window(node: Node<'_>, source: &[u8]) -> CodeWindow {
    CodeWindow {
        text: text(node, source).to_owned(),
        embedding_text: None,
        range: TextRange {
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            start_byte_offset: node.start_byte(),
            end_byte_offset: node.end_byte(),
        },
    }
}

fn split_large_node(
    node: Node<'_>,
    source: &[u8],
    max_chars: usize,
    overlap_chars: usize,
) -> Vec<CodeWindow> {
    let body = node.child_by_field_name("body").unwrap_or(node);
    let statements = named_children(body);
    if statements.len() <= 1 {
        return split_text_by_lines(
            text(node, source),
            max_chars,
            node.start_position().row + 1,
            node.start_byte(),
            overlap_chars,
        );
    }

    let mut windows = Vec::new();
    let mut group_start = 0;
    let mut group_chars = 0;
    for (index, statement) in statements.iter().copied().enumerate() {
        let statement_chars = utf16_len(text(statement, source));
        if statement_chars > max_chars {
            if index > group_start {
                windows.push(slice_statements(
                    source,
                    &statements,
                    group_start,
                    index - 1,
                ));
            }
            windows.extend(split_text_by_lines(
                text(statement, source),
                max_chars,
                statement.start_position().row + 1,
                statement.start_byte(),
                overlap_chars,
            ));
            group_start = index + 1;
            group_chars = 0;
            continue;
        }

        let separator_chars = usize::from(index > group_start);
        if group_chars + separator_chars + statement_chars > max_chars && index > group_start {
            windows.push(slice_statements(
                source,
                &statements,
                group_start,
                index - 1,
            ));
            let overlap_start =
                compute_overlap_start(source, &statements, group_start, index - 1, overlap_chars);
            let mut candidate_start = if overlap_start < index {
                overlap_start
            } else {
                index
            };
            let mut candidate_chars = statement_chars;
            for previous in (candidate_start..index).rev() {
                let added_chars = utf16_len(text(statements[previous], source)) + 1;
                if candidate_chars + added_chars > max_chars {
                    candidate_start = previous + 1;
                    break;
                }
                candidate_chars += added_chars;
            }
            group_start = candidate_start;
            group_chars = candidate_chars;
            continue;
        }
        group_chars += separator_chars + statement_chars;
    }

    if group_start < statements.len() {
        windows.push(slice_statements(
            source,
            &statements,
            group_start,
            statements.len() - 1,
        ));
    }
    windows
}

fn slice_statements(
    source: &[u8],
    statements: &[Node<'_>],
    start_index: usize,
    end_index: usize,
) -> CodeWindow {
    let start = statements[start_index].start_byte();
    let end = statements[end_index].end_byte();
    CodeWindow {
        text: String::from_utf8_lossy(&source[start..end]).into_owned(),
        embedding_text: Some(
            statements[start_index..=end_index]
                .iter()
                .map(|statement| text(*statement, source))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        range: TextRange {
            start_line: statements[start_index].start_position().row + 1,
            end_line: statements[end_index].end_position().row + 1,
            start_byte_offset: start,
            end_byte_offset: end,
        },
    }
}

fn split_text_by_lines(
    value: &str,
    max_chars: usize,
    start_line: usize,
    start_byte_offset: usize,
    overlap_chars: usize,
) -> Vec<CodeWindow> {
    let lines = value.split('\n').collect::<Vec<_>>();
    let line_offsets = line_byte_offsets(&lines);
    let mut windows = Vec::new();
    let mut line_index = 0;

    while line_index < lines.len() {
        if utf16_len(lines[line_index]) > max_chars {
            windows.extend(split_long_line_by_chars(
                lines[line_index],
                max_chars,
                start_line + line_index,
                start_byte_offset + line_offsets[line_index],
                overlap_chars,
            ));
            line_index += 1;
            continue;
        }

        let mut end_index = line_index;
        let mut used_chars = 0;
        while end_index < lines.len() {
            let line_length = utf16_len(lines[end_index]) + 1;
            if used_chars + line_length > max_chars && end_index > line_index {
                break;
            }
            used_chars += line_length;
            end_index += 1;
        }
        let chunk = lines[line_index..end_index].join("\n");
        windows.push(CodeWindow {
            text: chunk.clone(),
            embedding_text: None,
            range: TextRange {
                start_line: start_line + line_index,
                end_line: start_line + end_index - 1,
                start_byte_offset: start_byte_offset + line_offsets[line_index],
                end_byte_offset: start_byte_offset + line_offsets[line_index] + chunk.len(),
            },
        });
        if end_index >= lines.len() {
            break;
        }
        let overlap_lines = compute_line_overlap(&lines, line_index, end_index, overlap_chars);
        line_index = end_index - overlap_lines;
    }
    windows
}

fn split_long_line_by_chars(
    line: &str,
    max_chars: usize,
    line_number: usize,
    start_byte_offset: usize,
    overlap_chars: usize,
) -> Vec<CodeWindow> {
    let total_chars = utf16_len(line);
    let mut windows = Vec::new();
    let mut start_char = 0;
    while start_char < total_chars {
        let end_char = (start_char + max_chars).min(total_chars);
        let start_byte = byte_offset_at_utf16_ceil(line, start_char);
        let actual_start = utf16_len(&line[..start_byte]);
        let mut end_byte = byte_offset_at_utf16_floor(line, end_char);
        if end_byte == start_byte {
            end_byte = byte_offset_at_utf16_ceil(line, end_char);
        }
        let actual_end = utf16_len(&line[..end_byte]);
        windows.push(CodeWindow {
            text: line[start_byte..end_byte].to_owned(),
            embedding_text: None,
            range: TextRange {
                start_line: line_number,
                end_line: line_number,
                start_byte_offset: start_byte_offset + start_byte,
                end_byte_offset: start_byte_offset + end_byte,
            },
        });
        if actual_end >= total_chars {
            break;
        }
        start_char = (actual_end.saturating_sub(overlap_chars)).max(actual_start + 1);
    }
    windows
}

fn compute_overlap_start(
    source: &[u8],
    statements: &[Node<'_>],
    group_start: usize,
    group_end: usize,
    overlap_chars: usize,
) -> usize {
    if overlap_chars == 0 {
        return group_end + 1;
    }
    let mut chars = 0;
    let mut index = group_end;
    loop {
        chars += utf16_len(text(statements[index], source));
        if index < group_end {
            chars += 1;
        }
        if index == group_start || chars >= overlap_chars {
            return index;
        }
        index -= 1;
    }
}

fn compute_line_overlap(
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
    for index in (start_index..end_index).rev() {
        chars += utf16_len(lines[index]) + 1;
        if chars > overlap_chars {
            break;
        }
        count += 1;
    }
    count.min((end_index - start_index) / 2)
}

fn code_entity_outline(
    entity: &CodeEntity<'_>,
    adapter: &LanguageAdapter,
    source: &[u8],
    max_chars: usize,
) -> String {
    let header = extract_code_header(text(entity.node, source));
    let mut lines = vec![if header.is_empty() {
        entity
            .name
            .clone()
            .unwrap_or_else(|| symbol_type_name(entity.symbol_type).to_owned())
    } else {
        header
    }];

    if matches!(
        entity.symbol_type,
        SymbolType::Class | SymbolType::Interface | SymbolType::Module
    ) {
        let members = collect_structure_outline_members(entity, adapter, source);
        if !members.is_empty() {
            lines.push(String::new());
            lines.push("members:".to_owned());
            lines.extend(
                members
                    .iter()
                    .map(|member| format!("- {}", format_outline_member(member))),
            );
        }
    } else if entity.symbol_type == SymbolType::Function {
        let calls = collect_function_call_names(entity.node, source);
        if !calls.is_empty() {
            lines.push(String::new());
            lines.push(format!("calls: {}", calls.join(", ")));
        }
    }
    truncate_outline(lines.join("\n").trim(), max_chars)
}

fn extract_code_header(value: &str) -> String {
    let mut lines = Vec::new();
    for line in value.lines().take(24) {
        lines.push(line);
        if line.contains('{') {
            break;
        }
    }
    let header = lines.join("\n").trim().to_owned();
    if utf16_len(&header) > 1_200 {
        format!("{}\n...", take_utf16(&header, 1_200).trim_end())
    } else {
        header
    }
}

#[derive(Debug)]
struct OutlineMember {
    symbol_type: SymbolType,
    name: Option<String>,
    signature: Option<String>,
}

fn collect_structure_outline_members(
    entity: &CodeEntity<'_>,
    adapter: &LanguageAdapter,
    source: &[u8],
) -> Vec<OutlineMember> {
    struct Collector<'tree, 'source, 'adapter> {
        root: Node<'tree>,
        adapter: &'adapter LanguageAdapter,
        source: &'source [u8],
        breadcrumb: &'adapter [String],
        members: Vec<OutlineMember>,
        seen: HashSet<String>,
    }

    impl Collector<'_, '_, '_> {
        fn visit(&mut self, current: Node<'_>, depth: usize) {
            if self.members.len() >= OUTLINE_MAX_MEMBERS || depth > 12 {
                return;
            }
            if !same_node(current, self.root) && self.adapter.is_entity(current) {
                for resolved in self.adapter.resolve_entities(current, self.source) {
                    if self.members.len() >= OUTLINE_MAX_MEMBERS || same_node(resolved, self.root) {
                        break;
                    }
                    let name = self.adapter.extract_name(resolved, self.source);
                    let symbol_type = self
                        .adapter
                        .classify(resolved, self.source, self.breadcrumb);
                    let signature = self.adapter.extract_signature(resolved, self.source);
                    let key = format!(
                        "{}:{}:{}:{}",
                        symbol_type_name(symbol_type),
                        name.as_deref().unwrap_or_default(),
                        signature.as_deref().unwrap_or_default(),
                        resolved.start_byte()
                    );
                    if self.seen.insert(key) {
                        self.members.push(OutlineMember {
                            symbol_type,
                            name,
                            signature,
                        });
                    }
                }
                return;
            }
            for child in named_children(current) {
                self.visit(child, depth + 1);
            }
        }
    }

    let mut collector = Collector {
        root: entity.node,
        adapter,
        source,
        breadcrumb: &entity.breadcrumb,
        members: Vec::new(),
        seen: HashSet::new(),
    };
    collector.visit(entity.node, 0);
    collector.members
}

fn format_outline_member(member: &OutlineMember) -> String {
    let name = member.name.as_deref().unwrap_or_default();
    let signature = member
        .signature
        .as_deref()
        .map(collapse_whitespace)
        .map(|value| truncate_outline(&value, OUTLINE_MAX_LINE_CHARS))
        .unwrap_or_default();
    let symbol = symbol_type_name(member.symbol_type);
    if !signature.is_empty() {
        if !name.is_empty() && !signature.contains(name) {
            format!("{symbol} {name}: {signature}")
        } else {
            format!("{symbol} {signature}")
        }
    } else if name.is_empty() {
        symbol.to_owned()
    } else {
        format!("{symbol} {name}")
    }
}

fn collect_function_call_names(node: Node<'_>, source: &[u8]) -> Vec<String> {
    fn visit(node: Node<'_>, source: &[u8], calls: &mut Vec<String>, seen: &mut HashSet<String>) {
        if calls.len() >= OUTLINE_MAX_CALLS {
            return;
        }
        if matches!(
            node.kind(),
            "call"
                | "call_expression"
                | "function_call_expression"
                | "method_invocation"
                | "object_creation_expression"
                | "new_expression"
        ) && let Some(name) = extract_call_name(node, source)
            && seen.insert(name.clone())
        {
            calls.push(name);
        }
        for child in named_children(node) {
            visit(child, source, calls, seen);
        }
    }

    let mut calls = Vec::new();
    let mut seen = HashSet::new();
    visit(node, source, &mut calls, &mut seen);
    calls
}

fn extract_call_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let target = node
        .child_by_field_name("function")
        .or_else(|| node.child_by_field_name("name"))
        .or_else(|| node.child_by_field_name("constructor"))
        .or_else(|| node.child_by_field_name("type"))
        .or_else(|| named_children(node).first().copied())?;
    let cleaned = collapse_whitespace(text(target, source));
    let cleaned = cleaned.strip_prefix("new ").unwrap_or(&cleaned).trim();
    if cleaned.is_empty()
        || utf16_len(cleaned) > OUTLINE_MAX_LINE_CHARS
        || cleaned.contains(['\n', '\r'])
        || !cleaned
            .chars()
            .any(|character| character.is_ascii_alphabetic() || matches!(character, '_' | '$'))
    {
        None
    } else {
        Some(cleaned.to_owned())
    }
}

fn truncate_outline(value: &str, max_chars: usize) -> String {
    if utf16_len(value) <= max_chars {
        value.to_owned()
    } else if max_chars <= 3 {
        ".".repeat(max_chars)
    } else {
        format!("{}...", take_utf16(value, max_chars - 3).trim_end())
    }
}

fn same_node(left: Node<'_>, right: Node<'_>) -> bool {
    left.start_byte() == right.start_byte()
        && left.end_byte() == right.end_byte()
        && left.kind() == right.kind()
}

fn code_entity_metadata(entity: &CodeEntity<'_>) -> EntityMetadata {
    EntityMetadata::Code {
        symbol_type: entity.symbol_type,
        symbol_name: entity.name.clone(),
        scope: (!entity.breadcrumb.is_empty()).then(|| entity.breadcrumb.join("::")),
        node_type: Some(normalized_node_type(entity.node.kind()).to_owned()),
        signature: entity.signature.clone(),
        documentation: entity.documentation.clone(),
        modifiers: entity.modifiers.clone(),
    }
}

fn normalized_node_type(node_type: &str) -> &str {
    // The native Go grammar renamed the same interface member node used by
    // tree-sitter-wasms. Keep persisted metadata compatible with main.
    if node_type == "method_elem" {
        "method_spec"
    } else {
        node_type
    }
}

#[derive(Debug)]
struct ScriptBlock<'source> {
    text: &'source str,
    format: FileFormat,
    jsx: bool,
    start_line: usize,
    start_byte_offset: usize,
}

fn extract_script_blocks(
    source: &TextSource,
    max_chars: usize,
    overlap_chars: usize,
) -> Result<Vec<IndexingExtractionFragment>, EngineError> {
    let mut fragments = Vec::new();
    for block in find_script_blocks(&source.text) {
        let mut block_source = source.clone();
        block_source.file.formats = vec![block.format];
        block.text.clone_into(&mut block_source.text);
        let block_fragments = extract_code(
            &block_source,
            ChunkOptions {
                max_chunk_chars: Some(max_chars),
                chunk_overlap_chars: Some(overlap_chars),
            },
            block.jsx,
        )?;
        let remapped = remap_script_block_fragments(
            &source.file.id,
            block_fragments,
            fragments.len(),
            block.start_line,
            block.start_byte_offset,
        );
        fragments.extend(remapped);
    }
    Ok(fragments)
}

fn find_script_blocks(value: &str) -> Vec<ScriptBlock<'_>> {
    let bytes = value.as_bytes();
    let mut blocks = Vec::new();
    let mut cursor = 0;
    while let Some(open) = find_ascii_case_insensitive(bytes, b"<script", cursor) {
        let after_name = open + b"<script".len();
        if bytes
            .get(after_name)
            .is_some_and(|byte| *byte != b'>' && !byte.is_ascii_whitespace())
        {
            cursor = after_name;
            continue;
        }
        let Some(tag_end_relative) = bytes[after_name..].iter().position(|byte| *byte == b'>')
        else {
            break;
        };
        let tag_end = after_name + tag_end_relative;
        let content_start = tag_end + 1;
        let Some(close) = find_ascii_case_insensitive(bytes, b"</script>", content_start) else {
            break;
        };
        let attrs = &value[after_name..tag_end];
        let (format, jsx) = script_block_format(attrs);
        blocks.push(ScriptBlock {
            text: &value[content_start..close],
            format,
            jsx,
            start_line: line_at_offset(bytes, content_start),
            start_byte_offset: content_start,
        });
        cursor = close + b"</script>".len();
    }
    blocks
}

fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    haystack
        .get(start..)?
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
        .map(|offset| start + offset)
}

fn line_at_offset(source: &[u8], offset: usize) -> usize {
    let mut line = 1;
    for byte in &source[..offset] {
        if *byte == b'\n' {
            line += 1;
        }
    }
    line
}

fn script_block_format(attrs: &str) -> (FileFormat, bool) {
    let bytes = attrs.as_bytes();
    let Some(position) = find_ascii_case_insensitive(bytes, b"lang", 0) else {
        return (FileFormat::JavaScript, false);
    };
    let mut index = position + 4;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if bytes.get(index) != Some(&b'=') {
        return (FileFormat::JavaScript, false);
    }
    index += 1;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    let quote = bytes
        .get(index)
        .copied()
        .filter(|byte| matches!(byte, b'\'' | b'"'));
    if quote.is_some() {
        index += 1;
    }
    let start = index;
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        index += 1;
    }
    match attrs[start..index].to_ascii_lowercase().as_str() {
        "ts" | "typescript" => (FileFormat::TypeScript, false),
        "tsx" => (FileFormat::TypeScript, true),
        "jsx" => (FileFormat::JavaScript, true),
        _ => (FileFormat::JavaScript, false),
    }
}

fn remap_script_block_fragments(
    file_id: &FileId,
    fragments: Vec<IndexingExtractionFragment>,
    start_index: usize,
    start_line: usize,
    start_byte_offset: usize,
) -> Vec<IndexingExtractionFragment> {
    let id_map = fragments
        .iter()
        .enumerate()
        .map(|(index, item)| {
            (
                item.fragment.document_id().to_owned(),
                make_entity_id(file_id, start_index + index),
            )
        })
        .collect::<HashMap<_, _>>();

    fragments
        .into_iter()
        .map(|mut item| {
            let id = id_map
                .get(item.fragment.document_id())
                .expect("fragment ID is registered")
                .clone();
            let range = match &mut item.fragment {
                EntityFragment::Standalone(entity) | EntityFragment::Representative(entity) => {
                    entity.id = id;
                    entity.file_id = file_id.clone();
                    &mut entity.range
                }
                EntityFragment::Window(window) => {
                    window.id = FragmentId::new(id.as_str()).expect("generated fragment id");
                    window.entity_id = id_map
                        .get(window.entity_id.as_str())
                        .expect("window owner is registered")
                        .clone();
                    window.file_id = file_id.clone();
                    &mut window.range
                }
            };
            if let SourceRange::Text(range) = range {
                range.start_line += start_line - 1;
                range.end_line += start_line - 1;
                range.start_byte_offset += start_byte_offset;
                range.end_byte_offset += start_byte_offset;
            }
            item
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::domain::{
        Content, EntityFragment, EntityMetadata, FileFormat, SourceRange, SymbolType, TextRange,
    };

    use super::super::test_content;

    use super::super::{
        ChunkOptions, extract, extract_for_indexing, test_source, vector_content_for_fragment,
    };

    fn named<'a>(fragments: &'a [super::EntityFragment], name: &str) -> &'a super::EntityFragment {
        fragments
            .iter()
            .find(|fragment| matches!(
                &fragment.metadata(),
                Some(EntityMetadata::Code { symbol_name: Some(candidate), .. }) if candidate == name
            ))
            .unwrap_or_else(|| panic!("expected fragment for {name}"))
    }

    fn assert_source_backed(source: &super::TextSource, fragment: &super::EntityFragment) {
        let Content::Text(content) = &test_content(fragment) else {
            panic!("text fragment expected");
        };
        let SourceRange::Text(TextRange {
            start_byte_offset,
            end_byte_offset,
            ..
        }) = *fragment.range()
        else {
            panic!("text range expected");
        };
        assert_eq!(
            source.text.get(start_byte_offset..end_byte_offset),
            Some(content.as_str())
        );
    }

    #[test]
    fn preserves_typescript_metadata_scope_and_source_ranges() {
        let source = test_source(
            FileFormat::TypeScript,
            "contract.ts",
            &[
                "/** Adds one. */",
                "async function add(value: number): Promise<number> {",
                "  return helper(value);",
                "}",
                "export function publish() { return add(1); }",
                "class Box {",
                "  private value = 1;",
                "  static create() { return new Box(); }",
                "}",
            ]
            .join("\n"),
        );
        let fragments = extract(
            &source,
            ChunkOptions {
                max_chunk_chars: Some(500),
                chunk_overlap_chars: Some(50),
            },
        )
        .expect("typescript extraction");
        let add = named(&fragments, "add");
        let publish = named(&fragments, "publish");
        let create = named(&fragments, "create");
        assert_source_backed(&source, add);
        assert_source_backed(&source, publish);
        assert_source_backed(&source, create);
        assert_eq!(
            add.metadata().cloned(),
            Some(EntityMetadata::Code {
                symbol_type: SymbolType::Function,
                symbol_name: Some("add".to_owned()),
                scope: None,
                node_type: Some("function_declaration".to_owned()),
                signature: Some("async function add(value: number): Promise<number>".to_owned()),
                documentation: Some("Adds one.".to_owned()),
                modifiers: vec!["async".to_owned()],
            })
        );
        assert!(matches!(
            &publish.metadata(),
            Some(EntityMetadata::Code { modifiers, .. }) if modifiers == &["exported"]
        ));
        assert_eq!(
            create.metadata().cloned(),
            Some(EntityMetadata::Code {
                symbol_type: SymbolType::Function,
                symbol_name: Some("create".to_owned()),
                scope: Some("Box".to_owned()),
                node_type: Some("method_definition".to_owned()),
                signature: Some("static create()".to_owned()),
                documentation: None,
                modifiers: vec!["static".to_owned()],
            })
        );
        assert!(matches!(
            *add.range(),
            SourceRange::Text(TextRange { start_line: 2, .. })
        ));
        assert!(matches!(
            *create.range(),
            SourceRange::Text(TextRange { start_line: 8, .. })
        ));
        assert_eq!(
            fragments
                .iter()
                .map(EntityFragment::document_id)
                .collect::<HashSet<_>>()
                .len(),
            fragments.len()
        );
        assert!(
            fragments
                .iter()
                .all(|fragment| fragment.document_id().len() == 64)
        );
        assert_eq!(
            fragments[0].document_id(),
            "be01deb2fd2d1004f29eef026b65afd85dedebc81db8eb7691e3d88116663b74"
        );
    }

    #[test]
    fn preserves_c_go_and_python_language_specific_metadata() {
        let c_source = test_source(
            FileFormat::C,
            "fixture.c",
            "typedef struct Widget { int value; } Widget;\nstatic int add(int a, int b) { return a + b; }",
        );
        let c = extract(&c_source, ChunkOptions::default()).expect("c extraction");
        assert_eq!(
            named(&c, "Widget").metadata().cloned(),
            Some(EntityMetadata::Code {
                symbol_type: SymbolType::Class,
                symbol_name: Some("Widget".to_owned()),
                scope: None,
                node_type: Some("type_definition".to_owned()),
                signature: Some("typedef struct Widget { int value; } Widget".to_owned()),
                documentation: None,
                modifiers: Vec::new(),
            })
        );
        assert!(matches!(
            &named(&c, "add").metadata(),
            Some(EntityMetadata::Code { modifiers, .. }) if modifiers == &["static"]
        ));

        let go_source = test_source(
            FileFormat::Go,
            "fixture.go",
            &[
                "package demo",
                "type Widget struct { value int }",
                "func (w *Widget) Value() int { return w.value }",
                "type Reader interface { Read() string }",
            ]
            .join("\n"),
        );
        let go = extract(&go_source, ChunkOptions::default()).expect("go extraction");
        assert_eq!(
            named(&go, "Value").metadata().cloned(),
            Some(EntityMetadata::Code {
                symbol_type: SymbolType::Function,
                symbol_name: Some("Value".to_owned()),
                scope: Some("Widget".to_owned()),
                node_type: Some("method_declaration".to_owned()),
                signature: Some("func (w *Widget) Value() int".to_owned()),
                documentation: None,
                modifiers: vec!["exported".to_owned()],
            })
        );
        assert!(matches!(
            &named(&go, "Reader").metadata(),
            Some(EntityMetadata::Code {
                symbol_type: SymbolType::Interface,
                ..
            })
        ));
        assert!(matches!(
            &named(&go, "Read").metadata(),
            Some(EntityMetadata::Code { scope: Some(scope), .. }) if scope == "Reader"
        ));

        let python_source = test_source(
            FileFormat::Python,
            "fixture.py",
            "class Service:\n    @staticmethod\n    async def fetch(value: str) -> str:\n        return value",
        );
        let python = extract(&python_source, ChunkOptions::default()).expect("python extraction");
        assert_eq!(
            named(&python, "fetch").metadata().cloned(),
            Some(EntityMetadata::Code {
                symbol_type: SymbolType::Function,
                symbol_name: Some("fetch".to_owned()),
                scope: Some("Service".to_owned()),
                node_type: Some("decorated_definition".to_owned()),
                signature: Some("async def fetch(value: str) -> str:".to_owned()),
                documentation: None,
                modifiers: vec!["async".to_owned(), "static".to_owned()],
            })
        );
    }

    #[test]
    fn supports_the_typescript_language_matrix() {
        let fixtures = [
            (
                FileFormat::C,
                "fixture.c",
                "int add(int a, int b) { return a + b; }",
                "add",
            ),
            (
                FileFormat::Cpp,
                "fixture.cpp",
                "class Widget { public: int value() { return 1; } };",
                "Widget",
            ),
            (
                FileFormat::Go,
                "fixture.go",
                "package main\nfunc Add(a int, b int) int { return a + b }",
                "Add",
            ),
            (
                FileFormat::Java,
                "fixture.java",
                "class Widget { public int value() { return 1; } }",
                "Widget",
            ),
            (
                FileFormat::Python,
                "fixture.py",
                "class Widget:\n    def value(self):\n        return 1",
                "Widget",
            ),
            (
                FileFormat::Rust,
                "fixture.rs",
                "pub struct Widget { value: i32 }\nimpl Widget { pub fn value(&self) -> i32 { self.value } }",
                "Widget",
            ),
            (
                FileFormat::JavaScript,
                "fixture.js",
                "/** docs */\nexport class Widget { static value() { return 1; } }",
                "Widget",
            ),
            (
                FileFormat::TypeScript,
                "fixture.tsx",
                "export function Widget() { return <div>Hello</div>; }",
                "Widget",
            ),
        ];
        for (format, path, source_text, expected) in fixtures {
            let source = test_source(format, path, source_text);
            let fragments = extract(
                &source,
                ChunkOptions {
                    max_chunk_chars: Some(500),
                    chunk_overlap_chars: Some(50),
                },
            )
            .unwrap_or_else(|error| panic!("{format:?}: {error}"));
            assert!(
                fragments.iter().any(|fragment| matches!(
                    &fragment.metadata(),
                    Some(EntityMetadata::Code { symbol_name: Some(name), .. }) if name == expected
                )),
                "{format:?} should expose {expected}"
            );
        }
    }

    #[test]
    fn large_entities_emit_outlines_grouped_windows_and_compact_embeddings() {
        let source = test_source(
            FileFormat::TypeScript,
            "large.ts",
            &[
                "export class Service {",
                "  first(value: string) { return value.repeat(20); }",
                "  second() { return this.first(fetchValue()); }",
                "  third() { return new Service(); }",
                "}",
                "export function orchestrate(value: string) {",
                "  const first = load(value);",
                "  const second = client.fetch(first);",
                "  return finalize(second);",
                "}",
            ]
            .join("\n"),
        );
        let prepared = extract_for_indexing(
            &source,
            ChunkOptions {
                max_chunk_chars: Some(140),
                chunk_overlap_chars: Some(30),
            },
        )
        .expect("large extraction");
        let service = prepared.iter().find(|item| {
            matches!(item.fragment, EntityFragment::Representative(_))
                && matches!(
                    &item.fragment.metadata(),
                    Some(EntityMetadata::Code { symbol_name: Some(name), .. }) if name == "Service"
                )
        }).expect("service outline");
        let Content::Text(service_text) = &test_content(&service.fragment) else {
            panic!("outline text expected");
        };
        assert!(service_text.contains("members:"));
        assert!(service_text.contains("function first(value: string)"));

        let function = prepared.iter().find(|item| {
            matches!(item.fragment, EntityFragment::Representative(_))
                && matches!(
                    &item.fragment.metadata(),
                    Some(EntityMetadata::Code { symbol_name: Some(name), .. }) if name == "orchestrate"
                )
        }).expect("function outline");
        let Content::Text(function_text) = &test_content(&function.fragment) else {
            panic!("outline text expected");
        };
        assert!(function_text.contains("calls: load, client.fetch, finalize"));

        for item in prepared.iter().filter(|item| {
            item.fragment.entity_id() == service.fragment.entity_id()
                && matches!(item.fragment, EntityFragment::Window(_))
        }) {
            assert_source_backed(&source, &item.fragment);
        }
    }

    #[test]
    fn compacts_ast_gaps_for_embedding_without_changing_stored_source() {
        let source = test_source(
            FileFormat::Python,
            "spaced.py",
            &[
                "def spaced() -> str:".to_owned(),
                "    first_value = prepare()".to_owned(),
            ]
            .into_iter()
            .chain((0..70).map(|_| String::new()))
            .chain([
                "    second_value = transform(first_value)".to_owned(),
                "    return second_value".to_owned(),
            ])
            .collect::<Vec<_>>()
            .join("\n"),
        );
        let prepared = extract_for_indexing(
            &source,
            ChunkOptions {
                max_chunk_chars: Some(120),
                chunk_overlap_chars: Some(18),
            },
        )
        .expect("python extraction");
        let compact = prepared
            .iter()
            .find(|item| {
                matches!(
                    item.embedding_source.as_deref(),
                    Some([Content::Text(value)])
                        if value.contains("first_value = prepare()")
                            && value.contains("second_value = transform(first_value)")
                )
            })
            .expect("compact embedding window");
        assert_source_backed(&source, &compact.fragment);
        let Some([Content::Text(embedding)]) = compact.embedding_source.as_deref() else {
            panic!("embedding text expected");
        };
        assert!(!embedding.contains("\n\n"));
        let vector = vector_content_for_fragment(
            &compact.fragment,
            compact.embedding_source.as_deref(),
            Some(120),
        );
        let [Content::Text(vector)] = vector.as_slice() else {
            panic!("vector text expected");
        };
        assert!(vector.chars().count() <= 120);
        assert!(vector.starts_with("symbol: function spaced"));
    }

    #[test]
    fn remaps_component_script_blocks_and_preserves_fallbacks() {
        let source = test_source(
            FileFormat::Svelte,
            "fixture.svelte",
            &[
                "<h1>你好 😀</h1>",
                "<script>",
                "export const first = () => 1;",
                "</script>",
                "<p>中间</p>",
                "<script lang=\"ts\">",
                "export function second(value: number) { return value; }",
                "</script>",
            ]
            .join("\r\n"),
        );
        let fragments = extract(&source, ChunkOptions::default()).expect("svelte extraction");
        let first = named(&fragments, "first");
        let second = named(&fragments, "second");
        assert_source_backed(&source, first);
        assert_source_backed(&source, second);
        assert!(matches!(
            first.range(),
            SourceRange::Text(TextRange { start_line: 3, .. })
        ));
        assert!(matches!(
            second.range(),
            SourceRange::Text(TextRange { start_line: 7, .. })
        ));

        let plain_script = test_source(
            FileFormat::Vue,
            "plain.vue",
            "<template>你好 😀</template>\r\n<script lang=\"ts\">\r\n// 没有声明\r\n</script>",
        );
        let fallback = extract(&plain_script, ChunkOptions::default()).expect("script fallback");
        assert_eq!(fallback.len(), 1);
        assert_source_backed(&plain_script, &fallback[0]);
        assert_eq!(
            test_content(&fallback[0]),
            Content::Text("\r\n// 没有声明\r\n".to_owned())
        );
        assert!(fallback[0].metadata().is_none());

        let no_script = test_source(FileFormat::Svelte, "plain.svelte", "<h1>No script</h1>");
        let fallback = extract(&no_script, ChunkOptions::default()).expect("component fallback");
        assert_eq!(fallback.len(), 1);
        assert_eq!(
            test_content(&fallback[0]),
            Content::Text(no_script.text.clone())
        );
        assert!(fallback[0].metadata().is_none());
    }

    #[test]
    fn unicode_windows_are_source_backed_and_character_bounded() {
        let source = test_source(
            FileFormat::TypeScript,
            "unicode.ts",
            &format!(
                "export function emoji() {{ return \"{}\"; }}",
                "😀".repeat(80)
            ),
        );
        for (max_chars, overlap_chars) in [(1, 0), (31, 7)] {
            let fragments = extract(
                &source,
                ChunkOptions {
                    max_chunk_chars: Some(max_chars),
                    chunk_overlap_chars: Some(overlap_chars),
                },
            )
            .expect("unicode extraction");
            let windows = fragments
                .iter()
                .filter(|fragment| {
                    matches!(
                        &fragment.metadata(),
                        Some(EntityMetadata::Code { symbol_name: Some(name), .. }) if name == "emoji"
                    ) && !matches!(fragment, EntityFragment::Representative(_))
                })
                .collect::<Vec<_>>();
            assert!(windows.len() > 2);
            for fragment in windows {
                assert_source_backed(&source, fragment);
                let Content::Text(content) = &test_content(fragment) else {
                    panic!("text expected");
                };
                assert!(content.chars().count() <= max_chars);
            }
        }
    }

    #[test]
    fn reports_utf8_byte_offsets_before_structured_entities() {
        let source = test_source(
            FileFormat::TypeScript,
            "offsets.ts",
            "const prefix = \"你好 😀\";\r\nexport function afterEmoji() { return true; }",
        );
        let fragments = extract(&source, ChunkOptions::default()).expect("offset extraction");
        let fragment = named(&fragments, "afterEmoji");
        assert_source_backed(&source, fragment);
        let SourceRange::Text(TextRange {
            start_byte_offset, ..
        }) = *fragment.range()
        else {
            panic!("text range expected");
        };
        assert_eq!(
            start_byte_offset,
            "const prefix = \"你好 😀\";\r\nexport ".len()
        );
    }

    #[test]
    fn unsupported_and_declaration_free_code_fall_back_to_plain_text() {
        for source in [
            test_source(FileFormat::Ruby, "fixture.rb", "puts 'hello'"),
            test_source(FileFormat::TypeScript, "plain.ts", "// no declarations"),
        ] {
            let fragments = extract(&source, ChunkOptions::default()).expect("fallback");
            assert_eq!(fragments.len(), 1);
            assert_eq!(
                test_content(&fragments[0]),
                Content::Text(source.text.clone())
            );
            assert!(fragments[0].metadata().is_none());
        }
    }
}
