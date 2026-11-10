use crate::{
    api::context::result::MatchedBy,
    domain::{
        ByteRange, Content, Entity, EntityFragment, EntityId, FileId, FileIndexStatus, FileRecord,
        FileSnapshot, FragmentId, Range, SourcePath,
    },
    pipelines::indexed_search::pipeline::{SearchEvidence, SearchHit},
};

fn hit_with_fragment(source: &str, start: usize, end: usize) -> SearchHit {
    let file_id = FileId::new(1);
    let content = Content::Code(source.to_owned());
    let id = EntityId::new(file_id, &content, &Range::Full).expect("entity ID");
    let fragment = EntityFragment {
        id: FragmentId::new(&id, 0),
        range: Range::Byte(ByteRange::new(start as u64, end as u64).expect("fragment offsets")),
    };
    // Keep the entity valid while allowing malformed search evidence in validation tests.
    let stored_fragment = EntityFragment {
        range: Range::Full,
        ..fragment.clone()
    };
    SearchHit {
        entity: Entity::new(
            id,
            file_id,
            Range::Full,
            content,
            None,
            vec![stored_fragment],
        )
        .expect("entity"),
        file: FileRecord::new(
            file_id,
            SourcePath::new("example.py").expect("source path"),
            FileSnapshot {
                size_bytes: source.len() as u64,
                modified_epoch_ms: None,
                content_hash: None,
            },
            FileIndexStatus::NotIndexed,
        )
        .expect("file"),
        evidence: vec![SearchEvidence { fragment }],
        rank: 1,
        score: 1.0,
        matched_by: MatchedBy::Fts,
        trace: None,
    }
}

#[test]
fn byte_fragments_preserve_complete_entities_and_exact_evidence() {
    for newline in ["\n", "\r\n"] {
        for (line, selected) in [
            ("            node.name,", "e.name,"),
            (
                "    @only_required_for_messages(\"invalid-metaclass\")",
                "ages(\"invalid-metaclass\")",
            ),
            ("    value = \"中😀βeta\"", "😀β"),
        ] {
            let source = format!("before{newline}{line}{newline}after");
            let start = source.find(selected).expect("selected text");
            let end = start + selected.len();
            let hit = hit_with_fragment(&source, start, end);
            let target = super::context_item_target(&hit).expect("source fragment");
            assert_eq!(target.content, source);
            let local = crate::utils::text_range_from_offsets(
                &source,
                &crate::utils::line_byte_offsets(&source),
                start,
                end,
            )
            .expect("matched range");
            assert_eq!(target.excerpt_range, Some(Range::Text(local).into()));
            assert_eq!(target.content_range.start_line(), Some(1));
            assert_eq!(target.content_range.last_line(), Some(3));
            assert_eq!(hit.entity.content(), &Content::Code(source));
        }
    }
}

#[test]
fn boundary_aligned_empty_and_invalid_fragments_are_preserved() {
    let source = "A中😀\r\nβeta\n尾";
    for (start, end) in [(0, 10), (10, 16), (16, 19), (0, 0), (12, 12), (19, 19)] {
        let hit = hit_with_fragment(source, start, end);
        let target = super::context_item_target(&hit).expect("valid fragment");
        assert_eq!(target.content, source);
        let selected = crate::utils::text_range_from_offsets(
            source,
            &crate::utils::line_byte_offsets(source),
            start,
            end,
        )
        .expect("matched range");
        assert_eq!(target.excerpt_range, Some(Range::Text(selected).into()));
    }
    for (start, end) in [(2, 12), (1, 11), (0, 999)] {
        let hit = hit_with_fragment(source, start, end);
        let error = super::context_item_target(&hit)
            .err()
            .expect("invalid fragment");
        assert_eq!(error.code(), crate::EngineError::STORAGE_FAILURE);
    }
}

#[test]
fn overlapping_python_fragments_preserve_the_entire_entity() {
    use crate::{
        domain::FileFormat,
        extraction::{ChunkOptions, TextSource, extract_for_indexing},
    };

    let source = TextSource {
        relative_path: SourcePath::new("example.py").expect("path"),
        formats: vec![FileFormat::Python],
        text: format!(
            "class Boundary:\n{}",
            "    def visit(self):\n        return self.attribute_name\n".repeat(80)
        ),
    };
    let extraction = extract_for_indexing(&source, ChunkOptions::default()).expect("extraction");
    let mut partial_starts = 0;
    for entity in extraction.fragments {
        let Content::Code(text) = entity.content else {
            continue;
        };
        for fragment in entity.fragments {
            let Range::Byte(bytes) = fragment.range else {
                continue;
            };
            let start = usize::try_from(bytes.start_offset()).expect("start");
            let end = usize::try_from(bytes.end_offset()).expect("end");
            let target = super::context_item_target(&hit_with_fragment(&text, start, end))
                .expect("source fragment");
            let selected = crate::utils::text_range_from_offsets(
                &text,
                &crate::utils::line_byte_offsets(&text),
                start,
                end,
            )
            .expect("fragment coordinates");
            partial_starts += usize::from(selected.start_byte_column() > 0);
            assert_eq!(target.content, text);
            assert_eq!(target.content_range.start_line(), Some(1));
            assert_eq!(target.content_range.last_line(), Some(text.lines().count()));
            assert_eq!(target.excerpt_range, Some(Range::Text(selected).into()));
        }
    }
    assert!(
        partial_starts > 0,
        "exercise actual overlapping mid-line fragments"
    );
}
