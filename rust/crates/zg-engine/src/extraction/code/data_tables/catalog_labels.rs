use tree_sitter::Node;

use super::unparenthesize;
use crate::{
    domain::{ByteRange, Range},
    extraction::ExtractedEntityFragment,
};

/// A forwarded string-keyed record catalog keeps its complete Value entity.
/// Index its literal keys, source-spelled labels and name references. Other
/// mappings keep the usual content fragments, including ordinary alias maps.
pub(super) fn fragments(
    declaration: Node<'_>,
    source: &str,
    max: usize,
) -> Option<Vec<ExtractedEntityFragment>> {
    let table = unparenthesize(declaration.named_child(0)?.child_by_field_name("right")?)?;
    let mut labels = Vec::new();
    let base = declaration.start_byte();
    let mut emit = |node: Node<'_>| -> Option<()> {
        if source[node.byte_range()].encode_utf16().nth(max).is_some() {
            return None;
        }
        labels.push(ExtractedEntityFragment {
            range: Range::Byte(
                ByteRange::new(
                    (node.start_byte() - base) as u64,
                    (node.end_byte() - base) as u64,
                )
                .expect("catalog label belongs to the declaration"),
            ),
        });
        Some(())
    };
    for pair in table
        .named_children(&mut table.walk())
        .filter(|n| !n.is_extra())
    {
        let key = plain_string(pair.child_by_field_name("key")?)?;
        emit(key)?;
        let record = unparenthesize(pair.child_by_field_name("value")?)?;
        if !matches!(record.kind(), "tuple" | "dictionary") {
            return None;
        }
        let mut pending = Vec::new();
        for field in record
            .named_children(&mut record.walk())
            .filter(|n| !n.is_extra())
        {
            if record.kind() == "dictionary" {
                plain_string(field.child_by_field_name("key")?)?;
                pending.push(field.child_by_field_name("value")?);
            } else {
                pending.push(field);
            }
        }
        if pending.len() < 2 {
            return None;
        }
        pending.reverse();
        let (mut has_prose, mut has_label) = (false, false);
        while let Some(field) = pending.pop() {
            match field.kind() {
                "string" => {
                    if let Some(content) = plain_string(field) {
                        let value = &source[content.byte_range()];
                        if identifier_like(value) {
                            emit(content)?;
                            has_label = true;
                        } else {
                            has_prose |= value.split_whitespace().nth(1).is_some();
                        }
                    }
                }
                "identifier" | "attribute" => {
                    // The table validator already accepted this bare or qualified name.
                    emit(field)?;
                    has_label = true;
                }
                "concatenated_string" => {
                    has_prose |= field
                        .named_children(&mut field.walk())
                        .filter_map(plain_string)
                        .any(|s| source[s.byte_range()].split_whitespace().nth(1).is_some());
                }
                // Index nested record values, leaving structural field names out.
                "pair" => pending.push(field.child_by_field_name("value")?),
                _ => {
                    let start = pending.len();
                    pending.extend(
                        field
                            .named_children(&mut field.walk())
                            .filter(|n| !n.is_extra()),
                    );
                    pending[start..].reverse();
                }
            }
        }
        if !has_prose || !has_label {
            return None;
        }
    }
    (!labels.is_empty()).then_some(labels)
}

fn plain_string(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    let mut parts = node
        .named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "string_start" | "string_end"));
    let content = parts.next()?;
    (content.kind() == "string_content" && parts.next().is_none()).then_some(content)
}

fn identifier_like(value: &str) -> bool {
    value.split(['-', '.']).all(|part| {
        let mut chars = part.chars();
        chars
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_alphabetic())
            && chars.all(|ch| ch == '_' || ch.is_alphanumeric())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{Content, EntityMetadata, FileFormat, SymbolType},
        extraction::{ChunkOptions, ExtractedEntity, code, test_source},
    };

    fn extract_table(declaration: &str, usage: &str) -> ExtractedEntity {
        let source = test_source(
            FileFormat::Python,
            "library.py",
            &format!("# header 🍇\n{declaration}\nclass Owner:\n    {usage}\n"),
        );
        let output = code::extract_for_indexing(&source, ChunkOptions::default(), true)
            .expect("table extraction");
        let table = output.fragments.into_iter().find(|entity|
            matches!(&entity.metadata, Some(EntityMetadata::Code(m)) if m.symbol_name.as_deref() == Some("TABLE") && m.symbol_type == Some(SymbolType::Value))
        ).expect("original table entity");
        assert_eq!(table.content, Content::Code(declaration.to_owned()));
        table
    }

    #[test]
    fn forwarded_record_catalog_projects_exact_key_and_value_source_ranges() {
        let declaration = concat!(
            "TABLE = {\n",
            "  'E1': ('🍇 long message', 'not-callable', cfg.labels.EXAMPLE),\n",
            "  'E2': {'summary': 'another message', 'name': 'a.b'},\n}"
        );
        let table = extract_table(declaration, "msgs = {**TABLE}");
        let texts: Vec<_> = table
            .fragments
            .iter()
            .map(|fragment| {
                let Range::Byte(r) = fragment.range else {
                    panic!("expected catalog source range")
                };
                &declaration[usize::try_from(r.start_offset()).expect("start")
                    ..usize::try_from(r.end_offset()).expect("end")]
            })
            .collect();
        assert_eq!(
            texts,
            ["E1", "not-callable", "cfg.labels.EXAMPLE", "E2", "a.b"]
        );
    }

    #[test]
    fn ordinary_and_label_free_tables_keep_their_normal_fragments() {
        for (declaration, usage) in [
            ("TABLE = {'a': 'an-alias'}", "items = TABLE"),
            ("TABLE = {'a': ('name', 'another-name')}", "items = TABLE"),
            (
                "TABLE = {'a': ('some prose', 'some more prose')}",
                "items = TABLE",
            ),
            ("TABLE = {'a': ('some prose', 'a-label')}", "items = None"),
        ] {
            let table = extract_table(declaration, usage);
            assert_eq!(table.fragments.len(), 1);
            assert_eq!(table.fragments[0].range, Range::Full);
        }
    }
}
