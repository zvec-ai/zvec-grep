use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use crate::{
    api::context::result::{
        ContentRange, ContextContainer, ContextItem, ContextItemKind,
        StructureEnrichmentDiagnostics, StructureEnrichmentSource,
    },
    domain::{
        EntityFragment, EntityMetadata, FileCategory, FileFormat, FileId, FileSnapshot, SourceFile,
        SourceRange,
    },
    extraction::{ChunkOptions, TextSource, extract},
    utils::{decode_text, sha256_hex},
};

pub(crate) const RG_STRUCTURE_ENRICH_FILE_LIMIT: usize = 100;
const STRUCTURE_FILE_ID_NAMESPACE: &str = "__rg_structure__";

pub(crate) struct StructureEnrichmentResult {
    pub items: Vec<ContextItem>,
    pub diagnostics: StructureEnrichmentDiagnostics,
}

pub(crate) fn enrich_lexical_items_with_structure(
    root: &Path,
    items: Vec<ContextItem>,
    max_file_size_bytes: Option<u64>,
) -> StructureEnrichmentResult {
    let matched_files = unique_lexical_file_paths(&items);
    let mut fragments_by_file = HashMap::new();
    let mut parsed_files = 0;
    for path in matched_files.iter().take(RG_STRUCTURE_ENRICH_FILE_LIMIT) {
        let fragments = parse_structural_fragments(root, path, max_file_size_bytes);
        if fragments.is_some() {
            parsed_files += 1;
        }
        fragments_by_file.insert(path.clone(), fragments);
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
        if item.kind == ContextItemKind::LexicalMatch
            && let Some(fragments) = fragments_by_file
                .get(&item.absolute_path)
                .and_then(Option::as_deref)
            && let Some(range) = lexical_match_range(&item)
            && let Some(container) = smallest_containing_fragment(fragments, range)
        {
            enriched_items += 1;
            enriched_files.insert(item.absolute_path.clone());
            item.metadata = container.metadata().map(Into::into).or(item.metadata);
            item.container = Some(ContextContainer {
                entity_id: container.entity_id().as_str().to_owned(),
                range: container.range().into(),
                metadata: container.metadata().map(Into::into),
            });
        }
        enriched.push(item);
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

fn unique_lexical_file_paths(items: &[ContextItem]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    items
        .iter()
        .filter(|item| item.kind == ContextItemKind::LexicalMatch)
        .filter(|item| seen.insert(item.absolute_path.clone()))
        .map(|item| item.absolute_path.clone())
        .collect()
}

fn parse_structural_fragments(
    root: &Path,
    absolute_path: &Path,
    explicit_max_size: Option<u64>,
) -> Option<Vec<EntityFragment>> {
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
    let (source_root, relative_path) = match absolute_path.strip_prefix(root) {
        Ok(relative) if !relative.as_os_str().is_empty() => (root, relative),
        _ => (
            absolute_path.parent()?,
            Path::new(absolute_path.file_name()?),
        ),
    };
    let source = TextSource {
        file: SourceFile {
            id: FileId::new(structure_file_id(absolute_path)).ok()?,
            absolute_path: absolute_path.to_path_buf(),
            relative_path: relative_path.to_path_buf(),
            root_path: source_root.to_path_buf(),
            formats,
            snapshot: FileSnapshot {
                size_bytes: metadata.len(),
                modified_epoch_ms: metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .and_then(|duration| duration.as_millis().try_into().ok()),
                content_hash: None,
            },
        },
        text,
    };
    let structural = extract(&source, ChunkOptions::default())
        .ok()?
        .into_iter()
        .filter(|fragment| fragment.metadata().is_some())
        .collect::<Vec<_>>();
    (!structural.is_empty()).then_some(structural)
}

fn lexical_match_range(item: &ContextItem) -> Option<&ContentRange> {
    let range = item.excerpt_range.as_ref().unwrap_or(&item.range);
    matches!(range, ContentRange::Text { .. }).then_some(range)
}

fn smallest_containing_fragment<'fragment>(
    fragments: &'fragment [EntityFragment],
    inner: &ContentRange,
) -> Option<&'fragment EntityFragment> {
    fragments
        .iter()
        .filter(|fragment| text_range_contains(fragment.range(), inner))
        .min_by(|left, right| compare_fragment_container(left, right))
}

fn text_range_contains(outer: &SourceRange, inner: &ContentRange) -> bool {
    matches!(
        (outer, inner),
        (
            SourceRange::Text(crate::domain::TextRange {
                start_line: outer_start,
                end_line: outer_end,
                ..
            }),
            ContentRange::Text {
                start_line: inner_start,
                end_line: inner_end,
                ..
            }
        ) if outer_start <= inner_start && outer_end >= inner_end
    )
}

fn compare_fragment_container(left: &EntityFragment, right: &EntityFragment) -> std::cmp::Ordering {
    fragment_line_span(left)
        .cmp(&fragment_line_span(right))
        .then_with(|| fragment_specificity(right).cmp(&fragment_specificity(left)))
        .then_with(|| left.document_id().cmp(right.document_id()))
}

fn fragment_line_span(fragment: &EntityFragment) -> usize {
    match fragment.range() {
        SourceRange::Text(range) => range.end_line.saturating_sub(range.start_line),
        _ => usize::MAX,
    }
}

fn fragment_specificity(fragment: &EntityFragment) -> u8 {
    match fragment.metadata() {
        Some(EntityMetadata::Code { symbol_name, .. }) => {
            if symbol_name.is_some() {
                2
            } else {
                1
            }
        }
        Some(EntityMetadata::Markdown { heading, .. }) => u8::from(heading.is_some()),
        None => 0,
    }
}

fn structure_file_id(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    sha256_hex(format!("{STRUCTURE_FILE_ID_NAMESPACE}\0{normalized}").as_bytes())
}

fn lexical_item_key(item: &ContextItem) -> String {
    format!("{}:{:?}", item.absolute_path.display(), item.range)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::tempdir;

    use crate::api::context::result::{
        ContentRange, ContextContentRole, ContextItem, ContextItemKind, ContextItemStatus,
        EntityMetadata, MatchedBy, StructureEnrichmentSource,
    };

    use super::enrich_lexical_items_with_structure;

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

        let result = enrich_lexical_items_with_structure(
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
            Some(EntityMetadata::Code { symbol_name, .. }) if symbol_name.as_deref() == Some("greet")
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
            let result = enrich_lexical_items_with_structure(
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
        }
    }

    #[test]
    fn honors_the_explicit_structure_file_size_limit() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("section.md");
        fs::write(&path, "# Large section\n\nneedle\n").expect("markdown fixture");
        let items = vec![lexical_item(directory.path(), "section.md", 3, "needle")];

        let enriched = enrich_lexical_items_with_structure(directory.path(), items.clone(), None);
        assert_eq!(enriched.diagnostics.parsed_files, 1);
        assert!(matches!(
            enriched.items[0]
                .container
                .as_ref()
                .and_then(|container| container.metadata.as_ref()),
            Some(EntityMetadata::Markdown { heading, .. }) if heading.as_deref() == Some("Large section")
        ));

        let skipped = enrich_lexical_items_with_structure(directory.path(), items, Some(8));
        assert_eq!(skipped.diagnostics.parsed_files, 0);
        assert!(skipped.items[0].container.is_none());
    }

    fn lexical_item(root: &Path, relative: &str, line: usize, content: &str) -> ContextItem {
        ContextItem {
            kind: ContextItemKind::LexicalMatch,
            rank: 0,
            absolute_path: root.join(relative),
            relative_path: relative.into(),
            range: ContentRange::Text {
                start_line: line,
                end_line: line,
                start_offset: 0,
                end_offset: content.len(),
            },
            excerpt_range: None,
            content: content.to_owned(),
            content_role: Some(ContextContentRole::Source),
            outline: None,
            status: ContextItemStatus::Fresh,
            score: None,
            matched_by: MatchedBy::Lexical,
            metadata: None,
            entity_id: None,
            container: None,
            trace: None,
            query_groups: Vec::new(),
            selection_reason: None,
            coverage_group: None,
        }
    }

    fn item_by_relative_path<'item>(items: &'item [ContextItem], path: &str) -> &'item ContextItem {
        items
            .iter()
            .find(|item| item.relative_path == Path::new(path))
            .expect("lexical item")
    }
}
