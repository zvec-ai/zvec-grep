use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use crate::{
    api::context::result::{StructureEnrichmentDiagnostics, StructureEnrichmentSource},
    domain::{
        CodeMetadata, EntityId, EntityMetadata, FileCategory, FileFormat, MarkdownMetadata, Range,
        SourcePath, TextRange,
    },
    extraction::{ChunkOptions, ExtractedEntity, TextSource, extract},
    utils::{decode_text, line_byte_offsets, sha256_hex_parts},
};

use crate::lexical::types::LexicalMatch;

pub(crate) const RG_STRUCTURE_ENRICH_FILE_LIMIT: usize = 100;
const STRUCTURE_NAMESPACE: &str = "__rg_structure__";

pub(crate) struct StructureEnrichmentResult {
    pub items: Vec<EnrichedLexicalMatch>,
    pub diagnostics: StructureEnrichmentDiagnostics,
}

pub(crate) struct EnrichedLexicalMatch {
    pub matched: LexicalMatch,
    pub container: Option<LexicalContainer>,
}

pub(crate) struct LexicalContainer {
    pub entity_id: EntityId,
    pub range: Range,
    pub metadata: Option<EntityMetadata>,
}

struct StructuralFragment {
    entity_index: usize,
    entity_id: EntityId,
    document_id: String,
    range: Range,
}

struct StructuralSource {
    fragments: Vec<StructuralFragment>,
    metadata: HashMap<usize, EntityMetadata>,
}

pub(crate) fn enrich_lexical_matches_with_structure(
    root: &Path,
    items: Vec<LexicalMatch>,
    max_file_size_bytes: Option<u64>,
) -> StructureEnrichmentResult {
    let matched_files = unique_lexical_file_paths(&items);
    let mut sources_by_file = HashMap::new();
    let mut parsed_files = 0;
    for path in matched_files.iter().take(RG_STRUCTURE_ENRICH_FILE_LIMIT) {
        let source = parse_structural_source(root, path, max_file_size_bytes);
        if source.is_some() {
            parsed_files += 1;
        }
        sources_by_file.insert(path.clone(), source);
    }

    let mut enriched_items = 0;
    let mut enriched_files = HashSet::new();
    let mut enriched = Vec::with_capacity(items.len());
    let mut seen = HashSet::new();
    for mut item in items {
        if !seen.insert(lexical_item_key(&item)) {
            continue;
        }
        item.rank = enriched.len() + 1;
        let container = if let Some(source) = sources_by_file
            .get(&item.absolute_path)
            .and_then(Option::as_ref)
            && let Some(container) = smallest_containing_fragment(
                source,
                item.excerpt_range.as_ref().unwrap_or(&item.range),
            ) {
            enriched_items += 1;
            enriched_files.insert(item.absolute_path.clone());
            Some(LexicalContainer {
                entity_id: container.entity_id.clone(),
                range: container.range,
                metadata: source.metadata.get(&container.entity_index).cloned(),
            })
        } else {
            None
        };
        enriched.push(EnrichedLexicalMatch {
            matched: item,
            container,
        });
    }

    let matched_count = matched_files.len();
    StructureEnrichmentResult {
        items: enriched,
        diagnostics: StructureEnrichmentDiagnostics {
            source: StructureEnrichmentSource::StructuralExtraction,
            file_limit: RG_STRUCTURE_ENRICH_FILE_LIMIT,
            matched_files: matched_count,
            parsed_files,
            enriched_files: enriched_files.len(),
            enriched_items,
            skipped_files: matched_count.saturating_sub(parsed_files),
            truncated: matched_count > RG_STRUCTURE_ENRICH_FILE_LIMIT,
        },
    }
}

fn unique_lexical_file_paths(items: &[LexicalMatch]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    items
        .iter()
        .filter(|item| seen.insert(item.absolute_path.clone()))
        .map(|item| item.absolute_path.clone())
        .collect()
}

fn parse_structural_source(
    root: &Path,
    absolute_path: &Path,
    explicit_max_size: Option<u64>,
) -> Option<StructuralSource> {
    // Direct lexical searches may target a single file or files outside the requested root.
    // Use the filename as the relative path when the requested root is not a parent.
    let relative_path = match absolute_path.strip_prefix(root) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative,
        _ => Path::new(absolute_path.file_name()?),
    };
    let namespace = sha256_hex_parts([
        STRUCTURE_NAMESPACE.as_bytes(),
        b"\0",
        absolute_path.as_os_str().as_encoded_bytes(),
    ]);
    let metadata = fs::metadata(absolute_path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return None;
    }
    let formats = FileFormat::from_path(absolute_path).ok()?;
    let code = formats
        .iter()
        .any(|format| format.categories().contains(&FileCategory::Code));
    let markdown = formats.contains(&FileFormat::Markdown);
    if !code && !markdown {
        return None;
    }
    let maximum = explicit_max_size.unwrap_or(if code { 1_048_576 } else { 268_435_456 });
    if metadata.len() > maximum {
        return None;
    }
    let bytes = fs::read(absolute_path).ok()?;
    let text = decode_text(&bytes, true)?.into_owned();
    let source = TextSource {
        relative_path: SourcePath::new(relative_path).ok()?,
        formats,
        text,
    };
    let fragments = extract(&source, ChunkOptions::default()).ok()?;
    let structural = namespace_lexical_fragments(fragments, &namespace);
    if structural.fragments.is_empty() {
        return None;
    }
    Some(structural)
}

fn namespace_lexical_fragments(
    entities: Vec<ExtractedEntity>,
    namespace: &str,
) -> StructuralSource {
    let mut metadata = HashMap::new();
    let mut fragments = Vec::new();
    for entity in entities {
        let Some(value) = entity.metadata else {
            continue;
        };
        let entity_id = EntityId::new(sha256_hex_parts([
            namespace.as_bytes(),
            b"\0entity\0",
            &(entity.index as u64).to_le_bytes(),
        ]))
        .expect("SHA-256 produces a nonempty entity ID");
        metadata.insert(entity.index, value);
        // The full entity remains a structural container for matches spanning
        // several retrieval ranges. It does not create a retrieval fragment.
        fragments.push(StructuralFragment {
            entity_index: entity.index,
            entity_id: entity_id.clone(),
            document_id: entity_id.as_str().to_owned(),
            range: entity.source_range,
        });
        let crate::domain::Content::Text(content) = &entity.content else {
            continue;
        };
        let line_offsets = line_byte_offsets(&content.split('\n').collect::<Vec<_>>());
        for (ordinal, fragment) in entity.fragments.into_iter().enumerate() {
            let (Range::Text(source_range), Range::Byte(local_range)) =
                (entity.source_range, fragment.range)
            else {
                continue;
            };
            let Ok(range) = local_range
                .text_range(content, &line_offsets)
                .and_then(|local| local.within(source_range))
            else {
                continue;
            };
            fragments.push(StructuralFragment {
                entity_index: entity.index,
                entity_id: entity_id.clone(),
                document_id: sha256_hex_parts([
                    entity_id.as_str().as_bytes(),
                    b"\0fragment\0",
                    &(ordinal as u64).to_le_bytes(),
                ]),
                range: Range::Text(range),
            });
        }
    }
    StructuralSource {
        fragments,
        metadata,
    }
}

fn smallest_containing_fragment<'fragment>(
    source: &'fragment StructuralSource,
    inner: &TextRange,
) -> Option<&'fragment StructuralFragment> {
    source
        .fragments
        .iter()
        .filter(|fragment| matches!(&fragment.range, Range::Text(outer) if outer.contains(inner)))
        .min_by(|left, right| compare_fragment_container(source, left, right))
}

fn compare_fragment_container(
    source: &StructuralSource,
    left: &StructuralFragment,
    right: &StructuralFragment,
) -> std::cmp::Ordering {
    fragment_byte_span(left)
        .cmp(&fragment_byte_span(right))
        .then_with(|| {
            metadata_specificity(source.metadata.get(&right.entity_index)).cmp(
                &metadata_specificity(source.metadata.get(&left.entity_index)),
            )
        })
        .then_with(|| left.document_id.cmp(&right.document_id))
}

fn fragment_byte_span(fragment: &StructuralFragment) -> usize {
    match &fragment.range {
        Range::Text(range) => range
            .end_byte_offset()
            .saturating_sub(range.start_byte_offset()),
        _ => usize::MAX,
    }
}

fn metadata_specificity(metadata: Option<&EntityMetadata>) -> u8 {
    match metadata {
        Some(EntityMetadata::Code(CodeMetadata { symbol_name, .. })) => {
            if symbol_name.is_some() {
                2
            } else {
                1
            }
        }
        Some(EntityMetadata::Markdown(MarkdownMetadata { heading, .. })) => {
            u8::from(heading.is_some())
        }
        None => 0,
    }
}

fn lexical_item_key(item: &LexicalMatch) -> (PathBuf, String) {
    (item.absolute_path.clone(), format!("{:?}", item.range))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::tempdir;

    use crate::domain::{CodeMetadata, EntityMetadata, MarkdownMetadata, TextRange};
    use crate::lexical::types::LexicalMatch;

    use super::{
        EnrichedLexicalMatch, StructureEnrichmentSource, enrich_lexical_matches_with_structure,
    };

    #[test]
    fn enriches_only_code_and_markdown_structures() {
        let directory = tempdir().expect("temporary directory");
        let fixtures = [
            ("unsupported.rb", "puts \"hello\"\n"),
            ("plain.ts", "// no declarations\n"),
            ("plain.md", "markdown without headings\n"),
            (
                "structured.ts",
                "export function greet() {\n  return \"hello\";\n}\n",
            ),
        ];
        for (relative, content) in fixtures {
            fs::write(directory.path().join(relative), content).expect("fixture file");
        }

        let result = enrich_lexical_matches_with_structure(
            directory.path(),
            vec![
                lexical_item(directory.path(), "unsupported.rb", 1, "puts \"hello\""),
                lexical_item(directory.path(), "plain.ts", 1, "// no declarations"),
                lexical_item(directory.path(), "plain.md", 1, "markdown without headings"),
                lexical_item(directory.path(), "structured.ts", 2, "  return \"hello\";"),
            ],
            None,
        );

        let unsupported = item_by_relative_path(&result.items, "unsupported.rb");
        let plain_code = item_by_relative_path(&result.items, "plain.ts");
        let plain_markdown = item_by_relative_path(&result.items, "plain.md");
        let structured = item_by_relative_path(&result.items, "structured.ts");
        assert!(unsupported.container.is_none());
        assert!(plain_code.container.is_none());
        assert!(plain_markdown.container.is_none());
        assert!(matches!(
            structured
                .container
                .as_ref()
                .and_then(|container| container.metadata.as_ref()),
            Some(EntityMetadata::Code(CodeMetadata { symbol_name, .. })) if symbol_name.as_deref() == Some("greet")
        ));
        assert_eq!(
            result.diagnostics.source,
            StructureEnrichmentSource::StructuralExtraction
        );
        assert_eq!(result.diagnostics.matched_files, 4);
        assert_eq!(result.diagnostics.parsed_files, 1);
        assert_eq!(result.diagnostics.enriched_files, 1);
        assert_eq!(result.diagnostics.enriched_items, 1);
        assert_eq!(result.diagnostics.skipped_files, 3);
        assert!(!result.diagnostics.truncated);

        for root in [
            directory.path().join("structured.ts"),
            directory.path().join("elsewhere"),
        ] {
            let result = enrich_lexical_matches_with_structure(
                &root,
                vec![lexical_item(
                    directory.path(),
                    "structured.ts",
                    2,
                    "  return \"hello\";",
                )],
                None,
            );
            assert_eq!(result.diagnostics.enriched_items, 1, "{}", root.display());
            assert_eq!(result.diagnostics.parsed_files, 1, "{}", root.display());
            assert_eq!(
                result.items[0]
                    .container
                    .as_ref()
                    .map(|container| &container.entity_id),
                structured
                    .container
                    .as_ref()
                    .map(|container| &container.entity_id),
            );
        }
    }

    #[test]
    fn distinguishes_identical_relative_paths_in_different_source_roots() {
        let first = tempdir().expect("first source root");
        let second = tempdir().expect("second source root");
        let ids = [first.path(), second.path()].map(|root| {
            fs::write(root.join("section.md"), "# Section\n\nneedle\n").expect("markdown fixture");
            let result = enrich_lexical_matches_with_structure(
                root,
                vec![lexical_item(root, "section.md", 3, "needle")],
                None,
            );
            result.items[0]
                .container
                .as_ref()
                .expect("section container")
                .entity_id
                .clone()
        });
        assert_ne!(ids[0], ids[1]);
    }

    #[cfg(unix)]
    #[test]
    fn keeps_non_unicode_source_paths_distinct_during_enrichment() {
        use std::os::unix::ffi::OsStrExt;
        let directory = tempdir().expect("workspace");
        let text = "# Section\n\nneedle\n";
        fs::write(directory.path().join("template.md"), text).expect("template");
        let template = lexical_item(directory.path(), "template.md", 3, "needle");
        let paths = [b"section-\xff.md".as_slice(), b"section-\xfe.md".as_slice()]
            .map(|bytes| Path::new(std::ffi::OsStr::from_bytes(bytes)));
        let items = paths
            .iter()
            .map(|path| {
                let mut item = template.clone();
                item.relative_path = path.to_path_buf();
                item.absolute_path = directory.path().join(path);
                item
            })
            .collect::<Vec<_>>();
        assert_eq!(
            items[0].absolute_path.to_string_lossy(),
            items[1].absolute_path.to_string_lossy()
        );
        let result = enrich_lexical_matches_with_structure(directory.path(), items, None);
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.diagnostics.matched_files, 2);
        // macOS filesystems can reject non-Unicode names. Exercise the namespace
        // independently of whether such paths can be created on this host.
        let source = super::TextSource {
            relative_path: super::SourcePath::new("template.md").expect("source path"),
            formats: vec![super::FileFormat::Markdown],
            text: text.to_owned(),
        };
        let fragments =
            super::extract(&source, super::ChunkOptions::default()).expect("template structure");
        let identities = paths.map(|path| {
            let namespace = crate::utils::sha256_hex_parts([
                super::STRUCTURE_NAMESPACE.as_bytes(),
                b"\0",
                directory.path().join(path).as_os_str().as_encoded_bytes(),
            ]);
            super::namespace_lexical_fragments(fragments.clone(), &namespace).fragments[0]
                .entity_id
                .clone()
        });
        assert_ne!(identities[0], identities[1]);
    }

    #[test]
    fn enriches_window_matches_with_their_owners_metadata() {
        let directory = tempdir().expect("workspace");
        let line = "A long section contains the needle, 中文 😀, and text for several windows.";
        let text = format!(
            "# Earlier\r\n\r\nA preamble 😀.\r\n\r\n# Shared heading\r\n\r\n{}",
            format!("{line}\r\n").repeat(200)
        );
        fs::write(directory.path().join("section.md"), &text).expect("markdown fixture");
        let result = enrich_lexical_matches_with_structure(
            directory.path(),
            vec![lexical_item(directory.path(), "section.md", 150, line)],
            None,
        );
        let container = result.items[0]
            .container
            .as_ref()
            .expect("window container");
        assert!(matches!(
            &container.metadata,
            Some(EntityMetadata::Markdown(MarkdownMetadata { heading: Some(heading), .. }))
                if heading == "Shared heading"
        ));
        let crate::domain::Range::Text(range) = container.range else {
            panic!("text window expected");
        };
        assert!(range.start_line() > 6);
        assert!(range.contains(&result.items[0].matched.range));
    }

    #[test]
    fn honors_the_explicit_structure_file_size_limit() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("section.md");
        fs::write(&path, "# Large section\n\nneedle\n").expect("markdown fixture");
        let items = vec![lexical_item(directory.path(), "section.md", 3, "needle")];

        let enriched = enrich_lexical_matches_with_structure(directory.path(), items.clone(), None);
        assert_eq!(enriched.diagnostics.parsed_files, 1);
        assert!(matches!(
            enriched.items[0]
                .container
                .as_ref()
                .and_then(|container| container.metadata.as_ref()),
            Some(EntityMetadata::Markdown(MarkdownMetadata { heading, .. })) if heading.as_deref() == Some("Large section")
        ));

        let skipped = enrich_lexical_matches_with_structure(directory.path(), items, Some(8));
        assert_eq!(skipped.diagnostics.parsed_files, 0);
        assert!(skipped.items[0].container.is_none());
    }

    #[test]
    fn distinguishes_structures_on_the_same_line_after_unicode() {
        let directory = tempdir().expect("temporary directory");
        let text = "const prefix = \"😀\"; function alpha() { return \"one\"; } function beta() { return \"two\"; }\n";
        fs::write(directory.path().join("same-line.ts"), text).expect("code fixture");
        let items = ["one", "two"].map(|needle| {
            let mut item = lexical_item(directory.path(), "same-line.ts", 1, text.trim_end());
            let start = text.find(needle).expect("matched text");
            let end = start + needle.len();
            item.range =
                TextRange::from_coordinates(start, end, 1, 1, start, end).expect("matched range");
            item
        });
        let result = enrich_lexical_matches_with_structure(directory.path(), items.into(), None);
        assert_eq!(result.diagnostics.enriched_items, 2);
        for (item, expected) in result.items.iter().zip(["alpha", "beta"]) {
            assert!(matches!(
                item.container.as_ref().and_then(|container| container.metadata.as_ref()),
                Some(EntityMetadata::Code(CodeMetadata { symbol_name: Some(name), .. })) if name == expected
            ));
        }
    }

    #[test]
    fn enriches_the_match_inside_expanded_context_and_keeps_source_ranges() {
        let directory = tempdir().expect("temporary directory");
        let text = "function alpha() { return \"one\"; } function beta() { return \"two\"; }\n";
        fs::write(directory.path().join("context.ts"), text).expect("code fixture");
        let mut item = lexical_item(directory.path(), "context.ts", 1, text.trim_end());
        let start = text.find("two").expect("matched text");
        let excerpt = TextRange::from_coordinates(start, start + 3, 1, 1, start, start + 3)
            .expect("matched range");
        item.excerpt_range = Some(excerpt);
        let visible_range = item.range;

        let result =
            enrich_lexical_matches_with_structure(directory.path(), vec![item.clone(), item], None);

        assert_eq!(result.items.len(), 1);
        let enriched = &result.items[0];
        assert_eq!(enriched.matched.rank, 1);
        assert_eq!(enriched.matched.range, visible_range);
        assert_eq!(enriched.matched.excerpt_range, Some(excerpt));
        assert_eq!(enriched.matched.content, text.trim_end());
        let container = enriched.container.as_ref().expect("matched function");
        assert!(matches!(
            container.metadata.as_ref(),
            Some(EntityMetadata::Code(CodeMetadata { symbol_name: Some(name), .. })) if name == "beta"
        ));
        let crate::domain::Range::Text(range) = container.range else {
            panic!("text container range");
        };
        assert!(range.contains(&excerpt));
        assert!(!range.contains(&visible_range));
        assert_eq!(result.diagnostics.enriched_items, 1);
    }

    fn lexical_item(root: &Path, relative: &str, line: usize, content: &str) -> LexicalMatch {
        let text = fs::read_to_string(root.join(relative)).expect("fixture source");
        let start = text
            .split_inclusive('\n')
            .take(line - 1)
            .map(str::len)
            .sum();
        let range =
            TextRange::from_coordinates(start, start + content.len(), line, line, 0, content.len())
                .expect("fixture range");
        LexicalMatch {
            rank: 0,
            absolute_path: root.join(relative),
            relative_path: relative.into(),
            range,
            excerpt_range: None,
            content: content.to_owned(),
        }
    }

    fn item_by_relative_path<'item>(
        items: &'item [EnrichedLexicalMatch],
        path: &str,
    ) -> &'item EnrichedLexicalMatch {
        items
            .iter()
            .find(|item| item.matched.relative_path == Path::new(path))
            .expect("lexical item")
    }
}
