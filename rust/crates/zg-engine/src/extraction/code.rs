mod adapter;

use tree_sitter::{Language, Node, Parser};

use crate::{
    EngineError,
    domain::{CodeMetadata, Content, EntityMetadata, FileFormat, Range, SymbolType},
    utils::line_byte_offsets,
};

use self::adapter::{LanguageAdapter, named_children, resolve_adapter};
use super::{
    ChunkOptions, ExtractedEntity, TextRange, TextSource, chunk_options_for_metadata,
    chunking::text_fragments, text::extract_plain_text_entities, validate_formats,
};

const DEFAULT_CODE_CHUNK_CHARS: usize = 3_600;
const DEFAULT_CODE_CHUNK_OVERLAP_CHARS: usize = 540;
const COMPONENT_CODE_FORMATS: [FileFormat; 2] = [FileFormat::Vue, FileFormat::Svelte];

pub(super) fn extract_for_indexing(
    source: &TextSource,
    options: ChunkOptions,
) -> Result<Vec<ExtractedEntity>, EngineError> {
    let jsx = source
        .relative_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("tsx"));
    extract_code(source, options, jsx)
}

fn extract_code(
    source: &TextSource,
    options: ChunkOptions,
    jsx: bool,
) -> Result<Vec<ExtractedEntity>, EngineError> {
    if !super::service::is_code_source(&source.formats) {
        return Ok(Vec::new());
    }
    validate_formats(&source.formats)?;
    let (max_chars, overlap_chars) = resolve_options(options)?;

    if source
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

    let format = if source.formats.contains(&FileFormat::Cpp) {
        Some(FileFormat::Cpp)
    } else {
        source
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
        append_entity(source, &entity, max_chars, overlap_chars, &mut output);
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

fn fallback(source: &TextSource, max_chars: usize, overlap_chars: usize) -> Vec<ExtractedEntity> {
    extract_plain_text_entities(source, max_chars, overlap_chars)
}

#[derive(Debug)]
struct CodeEntity<'tree> {
    node: Node<'tree>,
    name: Option<String>,
    symbol_type: Option<SymbolType>,
    breadcrumb: Vec<String>,
    signature: Option<String>,
    documentation: Option<String>,
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
                    symbol_type: adapter.classify(entity),
                    breadcrumb: entity_breadcrumb,
                    signature: adapter.extract_signature(entity, source),
                    documentation: LanguageAdapter::extract_doc(entity, source),
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
    entity: &CodeEntity<'_>,
    max_chars: usize,
    overlap_chars: usize,
    output: &mut Vec<ExtractedEntity>,
) {
    let metadata = code_entity_metadata(entity);
    let (content_max, content_overlap) =
        chunk_options_for_metadata(max_chars, overlap_chars, Some(&metadata));
    let range = TextRange::from_coordinates(
        entity.node.start_byte(),
        entity.node.end_byte(),
        entity.node.start_position().row + 1,
        entity.node.end_position().row + 1,
        entity.node.start_position().column,
        entity.node.end_position().column,
    )
    .expect("parser coordinates refer to source text");
    let content = range
        .slice(&source.text)
        .expect("parser range is valid UTF-8")
        .to_owned();
    output.push(ExtractedEntity {
        index: output.len(),
        source_range: Range::Text(range),
        fragments: text_fragments(&content, content_max, content_overlap),
        content: Content::Text(content),
        metadata: Some(metadata),
    });
}

fn code_entity_metadata(entity: &CodeEntity<'_>) -> EntityMetadata {
    EntityMetadata::Code(CodeMetadata {
        symbol_type: entity.symbol_type,
        symbol_name: entity.name.clone(),
        scope: (!entity.breadcrumb.is_empty()).then(|| entity.breadcrumb.join("::")),
        signature: entity.signature.clone(),
        documentation: entity.documentation.clone(),
    })
}

#[derive(Debug)]
struct ScriptBlock<'source> {
    text: &'source str,
    format: FileFormat,
    jsx: bool,
    start_byte_offset: usize,
}

fn extract_script_blocks(
    source: &TextSource,
    max_chars: usize,
    overlap_chars: usize,
) -> Result<Vec<ExtractedEntity>, EngineError> {
    let lines = source.text.split('\n').collect::<Vec<_>>();
    let line_offsets = line_byte_offsets(&lines);
    let mut fragments = Vec::new();
    for block in find_script_blocks(&source.text) {
        let mut block_source = source.clone();
        block_source.formats = vec![block.format];
        block.text.clone_into(&mut block_source.text);
        let block_fragments = extract_code(
            &block_source,
            ChunkOptions {
                max_chunk_chars: Some(max_chars),
                chunk_overlap_chars: Some(overlap_chars),
            },
            block.jsx,
        )?;
        let remapped = remap_script_block_entities(
            source,
            block_fragments,
            fragments.len(),
            &line_offsets,
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

fn remap_script_block_entities(
    source: &TextSource,
    fragments: Vec<ExtractedEntity>,
    start_index: usize,
    line_offsets: &[usize],
    start_byte_offset: usize,
) -> Vec<ExtractedEntity> {
    fragments
        .into_iter()
        .map(|mut entity| {
            entity.index += start_index;
            if let Range::Text(range) = &mut entity.source_range {
                *range = TextRange::from_offsets(
                    &source.text,
                    line_offsets,
                    start_byte_offset + range.start_byte_offset(),
                    start_byte_offset + range.end_byte_offset(),
                )
                .expect("script coordinates refer to the full source");
            }
            entity
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::domain::{
        CodeMetadata, Content, EntityMetadata, FileFormat, Range, SymbolType, TextRange,
    };

    use super::super::{test_content, test_metadata};

    use super::super::{
        ChunkOptions, ExtractedEntity, extract, extract_for_indexing, test_source,
        vector_content_for_fragment,
    };

    fn named<'a>(
        fragments: &'a [super::ExtractedEntity],
        name: &str,
    ) -> &'a super::ExtractedEntity {
        fragments
            .iter()
            .find(|fragment| matches!(
                &test_metadata(fragment),
                Some(EntityMetadata::Code(CodeMetadata { symbol_name: Some(candidate), .. })) if candidate == name
            ))
            .unwrap_or_else(|| panic!("expected fragment for {name}"))
    }

    fn signature<'a>(fragments: &'a [super::ExtractedEntity], name: &str) -> &'a str {
        let Some(EntityMetadata::Code(metadata)) = test_metadata(named(fragments, name)) else {
            panic!("code metadata expected for {name}");
        };
        metadata.signature.as_deref().expect("signature expected")
    }

    fn assert_source_backed(source: &super::TextSource, entity: &super::ExtractedEntity) {
        let Content::Text(content) = &entity.content else {
            panic!("text entity expected");
        };
        let Range::Text(range) = entity.source_range else {
            panic!("text range expected");
        };
        assert_eq!(
            range.slice(&source.text).expect("entity source range"),
            content
        );
        let mut covered = vec![false; content.len()];
        for fragment in &entity.fragments {
            let (start, end) = match fragment.range {
                Range::Full => (0, content.len()),
                Range::Byte(local) => (
                    usize::try_from(local.start_offset).expect("fragment start"),
                    usize::try_from(local.end_offset).expect("fragment end"),
                ),
                Range::Text(_) => panic!("fragments store byte offsets without text coordinates"),
            };
            assert!(start < end && end <= content.len());
            let selected = source
                .text
                .get(range.start_byte_offset() + start..range.start_byte_offset() + end)
                .expect("source slice");
            assert!(
                !selected.trim().is_empty(),
                "fragments contain searchable source"
            );
            assert_eq!(
                fragment
                    .range
                    .extract(&entity.content)
                    .expect("content slice"),
                Content::Text(selected.to_owned())
            );
            covered[start..end].fill(true);
        }
        assert!(
            content.char_indices().all(|(offset, character)| {
                character.is_whitespace()
                    || covered[offset..offset + character.len_utf8()]
                        .iter()
                        .all(|value| *value)
            }),
            "fragments cover all non-whitespace source, including delimiters"
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
            test_metadata(add).cloned(),
            Some(EntityMetadata::Code(CodeMetadata {
                symbol_type: Some(SymbolType::Function),
                symbol_name: Some("add".to_owned()),
                scope: None,
                signature: Some("async function add(value: number): Promise<number>".to_owned()),
                documentation: Some("Adds one.".to_owned()),
            }))
        );
        assert!(matches!(
            &test_metadata(publish),
            Some(EntityMetadata::Code(CodeMetadata { signature: Some(signature), .. })) if signature == "export function publish()"
        ));
        assert_eq!(
            test_metadata(create).cloned(),
            Some(EntityMetadata::Code(CodeMetadata {
                symbol_type: Some(SymbolType::Function),
                symbol_name: Some("create".to_owned()),
                scope: Some("Box".to_owned()),
                signature: Some("static create()".to_owned()),
                documentation: None,
            }))
        );
        assert!(matches!(
            *add.source_range(),
            Range::Text(range) if range.start_line() == 2
        ));
        assert!(matches!(
            *create.source_range(),
            Range::Text(range) if range.start_line() == 8
        ));
        assert_eq!(
            fragments
                .iter()
                .map(ExtractedEntity::index)
                .collect::<HashSet<_>>()
                .len(),
            fragments.len()
        );
        assert!(
            fragments
                .iter()
                .enumerate()
                .all(|(index, fragment)| fragment.index() == index)
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
            test_metadata(named(&c, "Widget")).cloned(),
            Some(EntityMetadata::Code(CodeMetadata {
                symbol_type: Some(SymbolType::Class),
                symbol_name: Some("Widget".to_owned()),
                scope: None,
                signature: Some("typedef struct Widget {} Widget".to_owned()),
                documentation: None,
            }))
        );
        assert!(matches!(
            &test_metadata(named(&c, "add")),
            Some(EntityMetadata::Code(CodeMetadata { signature: Some(signature), .. })) if signature == "static int add(int a, int b)"
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
            test_metadata(named(&go, "Value")).cloned(),
            Some(EntityMetadata::Code(CodeMetadata {
                symbol_type: Some(SymbolType::Function),
                symbol_name: Some("Value".to_owned()),
                scope: Some("Widget".to_owned()),
                signature: Some("func (w *Widget) Value() int".to_owned()),
                documentation: None,
            }))
        );
        assert!(matches!(
            &test_metadata(named(&go, "Reader")),
            Some(EntityMetadata::Code(CodeMetadata {
                symbol_type: Some(SymbolType::Interface),
                ..
            }))
        ));
        assert!(matches!(
            &test_metadata(named(&go, "Read")),
            Some(EntityMetadata::Code(CodeMetadata { scope: Some(scope), .. })) if scope == "Reader"
        ));

        let python_source = test_source(
            FileFormat::Python,
            "fixture.py",
            "class Service:\n    @staticmethod\n    async def fetch(value: str) -> str:\n        return value",
        );
        let python = extract(&python_source, ChunkOptions::default()).expect("python extraction");
        assert_eq!(
            test_metadata(named(&python, "fetch")).cloned(),
            Some(EntityMetadata::Code(CodeMetadata {
                symbol_type: Some(SymbolType::Function),
                symbol_name: Some("fetch".to_owned()),
                scope: Some("Service".to_owned()),
                signature: Some("@staticmethod\nasync def fetch(value: str) -> str:".to_owned()),
                documentation: None,
            }))
        );
    }

    #[test]
    fn signatures_preserve_direct_exports_without_implementation_bodies() {
        let source = test_source(
            FileFormat::TypeScript,
            "signatures.ts",
            &[
                "export class Service {",
                "  static create() { return new Service(); }",
                "  private load = async (id: number) => { return id; };",
                "}",
                "export default async function start() { return new Service(); }",
                "export const read = (id: number) => id + 1, write = async () => { return 2; };",
                "const settings = { hidden: 'implementation' };",
                "export const wrapped = memo((id: number) => { return id; });",
                "export declare function declared(value: string): void;",
                "function configure(options = { mode: 'strict  mode' }) { return options; }",
            ]
            .join("\n"),
        );
        let fragments = extract(&source, ChunkOptions::default()).expect("typescript extraction");
        for (name, expected) in [
            ("Service", "export class Service"),
            ("create", "static create()"),
            ("load", "private load = async (id: number) =>"),
            ("start", "export default async function start()"),
            ("read", "export const read = (id: number) =>"),
            ("write", "export const write = async () =>"),
            ("settings", "const settings = {}"),
            ("wrapped", "export const wrapped = memo((id: number) => {})"),
            (
                "declared",
                "export declare function declared(value: string): void",
            ),
            (
                "configure",
                "function configure(options = { mode: 'strict  mode' })",
            ),
        ] {
            assert_eq!(signature(&fragments, name), expected, "{name}");
        }
    }

    #[test]
    fn signatures_preserve_python_decorators_and_their_arguments() {
        let source = test_source(
            FileFormat::Python,
            "signatures.py",
            &[
                "@registered(name=\"Service  API\")",
                "class Service:",
                "    @staticmethod",
                "    @route(\"a  b\")",
                "    async def fetch(value: str) -> str:",
                "        return value",
            ]
            .join("\n"),
        );
        let fragments = extract(&source, ChunkOptions::default()).expect("python extraction");
        assert_eq!(
            signature(&fragments, "Service"),
            "@registered(name=\"Service  API\")\nclass Service:"
        );
        assert_eq!(
            signature(&fragments, "fetch"),
            "@staticmethod\n@route(\"a  b\")\nasync def fetch(value: str) -> str:"
        );
    }

    #[test]
    fn signatures_preserve_rust_visibility_attributes_and_documentation() {
        let source = test_source(
            FileFormat::Rust,
            "signatures.rs",
            &[
                "#[must_use = \"use  the result\"]",
                "// Load a value.",
                "#[inline]",
                "/// Keep the result.",
                "pub(crate) async unsafe fn load(value: usize) -> usize { value }",
                "pub fn plain() {}",
            ]
            .join("\n"),
        );
        let fragments = extract(&source, ChunkOptions::default()).expect("rust extraction");
        assert_eq!(
            signature(&fragments, "load"),
            "#[must_use = \"use  the result\"]\n#[inline]\npub(crate) async unsafe fn load(value: usize) -> usize"
        );
        assert_eq!(signature(&fragments, "plain"), "pub fn plain()");
        assert!(matches!(
            test_metadata(named(&fragments, "load")),
            Some(EntityMetadata::Code(CodeMetadata { documentation: Some(doc), .. }))
                if doc == "Load a value.\nKeep the result."
        ));
    }

    #[test]
    fn classifies_enums_across_languages() {
        let fixtures = [
            (
                FileFormat::C,
                "enum State { READY };",
                "State",
                Some(SymbolType::Enum),
            ),
            (
                FileFormat::C,
                "typedef enum { READY } State;",
                "State",
                Some(SymbolType::Enum),
            ),
            (
                FileFormat::Cpp,
                "enum class State { Ready };",
                "State",
                Some(SymbolType::Enum),
            ),
            (
                FileFormat::Java,
                "enum State { READY }",
                "State",
                Some(SymbolType::Enum),
            ),
            (
                FileFormat::Rust,
                "enum State { Ready }",
                "State",
                Some(SymbolType::Enum),
            ),
            (
                FileFormat::TypeScript,
                "enum State { Ready }",
                "State",
                Some(SymbolType::Enum),
            ),
        ];

        for (format, text, name, expected) in fixtures {
            let source = test_source(format, "fixture", text);
            let fragments = extract(&source, ChunkOptions::default()).expect("symbol extraction");
            let Some(EntityMetadata::Code(metadata)) = test_metadata(named(&fragments, name))
            else {
                panic!("expected code metadata for {format:?} {name}");
            };
            assert_eq!(metadata.symbol_type, expected, "{format:?}: {text}");
        }
    }

    #[test]
    fn classifies_classes_interfaces_and_aliases() {
        let fixtures = [
            (
                FileFormat::Cpp,
                "struct User {};",
                "User",
                Some(SymbolType::Class),
            ),
            (
                FileFormat::Java,
                "record User(int id) {}",
                "User",
                Some(SymbolType::Class),
            ),
            (
                FileFormat::Go,
                "package demo\ntype User struct { id int }",
                "User",
                Some(SymbolType::Class),
            ),
            (
                FileFormat::Rust,
                "struct User { id: u64 }",
                "User",
                Some(SymbolType::Class),
            ),
            (
                FileFormat::Rust,
                "impl Read for User { fn read(&self) {} }",
                "User",
                Some(SymbolType::Class),
            ),
            (
                FileFormat::Java,
                "@interface Label {}",
                "Label",
                Some(SymbolType::Interface),
            ),
            (
                FileFormat::Rust,
                "trait Read { fn read(&self); }",
                "Read",
                Some(SymbolType::Interface),
            ),
            (
                FileFormat::TypeScript,
                "interface Read { read(): void; }",
                "Read",
                Some(SymbolType::Interface),
            ),
            (
                FileFormat::C,
                "typedef int UserID;",
                "UserID",
                Some(SymbolType::Alias),
            ),
            (
                FileFormat::Go,
                "package demo\ntype UserID = int",
                "UserID",
                Some(SymbolType::Alias),
            ),
            (
                FileFormat::Rust,
                "type UserID = u64;",
                "UserID",
                Some(SymbolType::Alias),
            ),
            (
                FileFormat::TypeScript,
                "type UserID = number;",
                "UserID",
                Some(SymbolType::Alias),
            ),
        ];

        for (format, text, name, expected) in fixtures {
            let source = test_source(format, "fixture", text);
            let fragments = extract(&source, ChunkOptions::default()).expect("symbol extraction");
            let Some(EntityMetadata::Code(metadata)) = test_metadata(named(&fragments, name))
            else {
                panic!("expected code metadata for {format:?} {name}");
            };
            assert_eq!(metadata.symbol_type, expected, "{format:?}: {text}");
        }
    }

    #[test]
    fn keeps_unclassified_type_declarations_without_value_fallback() {
        let fixtures = [
            (
                FileFormat::Go,
                "package demo\ntype UserID int",
                "UserID",
                None,
            ),
            (
                FileFormat::Rust,
                "union Data { integer: u32, float: f32 }",
                "Data",
                None,
            ),
            (
                FileFormat::C,
                "typedef union { int integer; float real; } Data;",
                "Data",
                None,
            ),
        ];

        for (format, text, name, expected) in fixtures {
            let source = test_source(format, "fixture", text);
            let fragments = extract(&source, ChunkOptions::default()).expect("symbol extraction");
            let Some(EntityMetadata::Code(metadata)) = test_metadata(named(&fragments, name))
            else {
                panic!("expected code metadata for {format:?} {name}");
            };
            assert_eq!(metadata.symbol_type, expected, "{format:?}: {text}");
        }
    }

    #[test]
    fn extracts_named_modules_and_values_without_local_variable_expansion() {
        let fixtures = [
            (
                FileFormat::Cpp,
                "namespace api { int run() { return 1; } }",
                "api",
                SymbolType::Module,
            ),
            (
                FileFormat::Rust,
                "mod api { pub fn run() {} }",
                "api",
                SymbolType::Module,
            ),
            (
                FileFormat::TypeScript,
                "namespace api { export function run() {} }",
                "api",
                SymbolType::Module,
            ),
            (
                FileFormat::TypeScript,
                "declare module 'api' { export function run(): void; }",
                "'api'",
                SymbolType::Module,
            ),
            (
                FileFormat::Rust,
                "const LIMIT: usize = 8;",
                "LIMIT",
                SymbolType::Value,
            ),
            (
                FileFormat::Rust,
                "static LIMIT: usize = 8;",
                "LIMIT",
                SymbolType::Value,
            ),
            (
                FileFormat::JavaScript,
                "const limit = 8;",
                "limit",
                SymbolType::Value,
            ),
            (
                FileFormat::TypeScript,
                "class User { name: string; }",
                "name",
                SymbolType::Value,
            ),
            (
                FileFormat::TypeScript,
                "interface User { name: string; }",
                "name",
                SymbolType::Value,
            ),
            (
                FileFormat::Java,
                "class User { int limit = 8; }",
                "limit",
                SymbolType::Value,
            ),
            (
                FileFormat::Java,
                "interface User { int LIMIT = 8; }",
                "LIMIT",
                SymbolType::Value,
            ),
        ];
        for (format, text, name, expected) in fixtures {
            let source = test_source(format, "fixture", text);
            let fragments = extract(&source, ChunkOptions::default()).expect("symbol extraction");
            assert!(
                matches!(
                    test_metadata(named(&fragments, name)),
                    Some(EntityMetadata::Code(CodeMetadata { symbol_type: Some(actual), .. })) if *actual == expected
                ),
                "{format:?}: {text}"
            );
        }

        let source = test_source(
            FileFormat::TypeScript,
            "locals.ts",
            "const limit = 8; function run() { const local = 1; return local; } const { nested } = input;",
        );
        let fragments = extract(&source, ChunkOptions::default()).expect("symbol extraction");
        let names = fragments
            .iter()
            .filter_map(|fragment| match test_metadata(fragment) {
                Some(EntityMetadata::Code(metadata)) => metadata.symbol_name.as_deref(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["limit", "run"]);
    }

    #[test]
    fn value_declarations_preserve_anonymous_class_methods() {
        for (format, text) in [
            (FileFormat::JavaScript, "const Worker = class { run() {} };"),
            (FileFormat::TypeScript, "const Worker = class { run() {} };"),
            (
                FileFormat::JavaScript,
                "class Host { Worker = class { run() {} }; }",
            ),
            (
                FileFormat::Java,
                "class Host { Object worker = new Object() { void run() {} }; }",
            ),
        ] {
            let source = test_source(format, "fixture", text);
            let fragments =
                extract(&source, ChunkOptions::default()).expect("anonymous class extraction");
            assert!(
                matches!(
                    test_metadata(named(&fragments, "run")),
                    Some(EntityMetadata::Code(CodeMetadata {
                        symbol_type: Some(SymbolType::Function),
                        ..
                    }))
                ),
                "{format:?}: {text}"
            );
        }
    }

    #[test]
    fn impl_class_keeps_method_metadata_and_complete_source() {
        let source = test_source(
            FileFormat::Rust,
            "fixture.rs",
            &format!(
                "impl User {{\n    fn name(&self) -> &str {{\n        \"{}\"\n    }}\n}}",
                "a".repeat(300)
            ),
        );
        let fragments = extract(
            &source,
            ChunkOptions {
                max_chunk_chars: Some(160),
                chunk_overlap_chars: Some(20),
            },
        )
        .expect("impl extraction");
        let owner = named(&fragments, "User");
        assert!(owner.fragments.len() > 1);
        assert!(matches!(
            test_metadata(owner),
            Some(EntityMetadata::Code(CodeMetadata {
                symbol_type: Some(SymbolType::Class),
                ..
            }))
        ));
        assert_source_backed(&source, owner);
        assert_eq!(owner.content, Content::Text(source.text.clone()));
        assert!(matches!(
            test_metadata(named(&fragments, "name")),
            Some(EntityMetadata::Code(CodeMetadata {
                symbol_type: Some(SymbolType::Function),
                ..
            }))
        ));
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
                    &test_metadata(fragment),
                    Some(EntityMetadata::Code(CodeMetadata { symbol_name: Some(name), .. })) if name == expected
                )),
                "{format:?} should expose {expected}"
            );
        }
    }

    #[test]
    fn long_entities_preserve_headers_delimiters_and_large_whitespace_gaps() {
        for (format, path, text) in [
            (
                FileFormat::TypeScript,
                "large.ts",
                format!(
                    "export function orchestrate() {{\r\n  load();\r\n{}  finalize();\r\n}}",
                    "\r\n".repeat(100)
                ),
            ),
            (
                FileFormat::Python,
                "spaced.py",
                format!(
                    "def spaced() -> str:\n    first_value = prepare()\n{}    return first_value",
                    "\n".repeat(100)
                ),
            ),
        ] {
            let source = test_source(format, path, &text);
            let entities = extract_for_indexing(
                &source,
                ChunkOptions {
                    max_chunk_chars: Some(120),
                    chunk_overlap_chars: Some(18),
                },
            )
            .expect("structured source");
            let entity = &entities[0];
            assert_source_backed(&source, entity);
            for fragment in &entity.fragments {
                let content = fragment
                    .range
                    .extract(&entity.content)
                    .expect("source slice");
                let vector =
                    vector_content_for_fragment(&content, entity.metadata.as_ref(), Some(120));
                let [Content::Text(vector)] = vector.as_slice() else {
                    panic!("text embedding");
                };
                assert!(crate::utils::utf16_len(vector) <= 120);
                assert!(vector.starts_with("symbol: function"));
            }
        }
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
            first.source_range(),
            Range::Text(range) if range.start_line() == 3
        ));
        assert!(matches!(
            second.source_range(),
            Range::Text(range) if range.start_line() == 7
        ));

        let inline_script = test_source(
            FileFormat::Vue,
            "inline.vue",
            "<p>你好 😀</p><script>export function inline() { return 1; }</script>",
        );
        let fragments =
            extract(&inline_script, ChunkOptions::default()).expect("inline script extraction");
        let inline = named(&fragments, "inline");
        assert_source_backed(&inline_script, inline);
        let Range::Text(range) = inline.source_range() else {
            panic!("text range expected");
        };
        assert_eq!(range.start_line(), 1);
        assert_eq!(
            range.start_byte_column(),
            "<p>你好 😀</p><script>export ".len()
        );

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
        assert!(test_metadata(&fallback[0]).is_none());
        assert!(matches!(
            fallback[0].source_range(),
            Range::Text(range) if range.end_line() == 4 && range.end_byte_column() == 0
        ));

        let no_script = test_source(FileFormat::Svelte, "plain.svelte", "<h1>No script</h1>");
        let fallback = extract(&no_script, ChunkOptions::default()).expect("component fallback");
        assert_eq!(fallback.len(), 1);
        assert_eq!(
            test_content(&fallback[0]),
            Content::Text(no_script.text.clone())
        );
        assert!(test_metadata(&fallback[0]).is_none());
    }

    #[test]
    fn preserves_local_fragment_ranges_across_component_script_blocks() {
        let body = (0..12)
            .map(|index| format!("  const value{index} = step({index});"))
            .collect::<Vec<_>>()
            .join("\r\n");
        let text = format!(
            "<p>你好 😀</p><script>export function first() {{\r\n{body}\r\n}}\r\n</script>\r\n\
             <script lang=\"ts\">export function second() {{\r\n{body}\r\n}}\r\n</script>"
        );
        for (format, path) in [
            (FileFormat::Vue, "fixture.vue"),
            (FileFormat::Svelte, "fixture.svelte"),
        ] {
            let source = test_source(format, path, &text);
            let fragments = extract(
                &source,
                ChunkOptions {
                    max_chunk_chars: Some(100),
                    chunk_overlap_chars: Some(10),
                },
            )
            .expect("component extraction");
            let first = named(&fragments, "first");
            let second = named(&fragments, "second");
            assert!(first.fragments.len() > 1);
            assert!(second.fragments.len() > 1);
            assert_ne!(first.index, second.index);
            for (index, entity) in fragments.iter().enumerate() {
                assert_eq!(entity.index, index);
                assert_source_backed(&source, entity);
            }
        }
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
            let entity = named(&fragments, "emoji");
            assert!(entity.fragments.len() > 2);
            assert_source_backed(&source, entity);
            for fragment in &entity.fragments {
                let Content::Text(content) = fragment
                    .range
                    .extract(&entity.content)
                    .expect("fragment content")
                else {
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
        let Range::Text(range) = *fragment.source_range() else {
            panic!("text range expected");
        };
        assert_eq!(
            range,
            TextRange::from_coordinates(
                "const prefix = \"你好 😀\";\r\nexport ".len(),
                source.text.len(),
                2,
                2,
                "export ".len(),
                "export function afterEmoji() { return true; }".len(),
            )
            .expect("function coordinates")
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
            assert!(test_metadata(&fragments[0]).is_none());
        }
    }
}
