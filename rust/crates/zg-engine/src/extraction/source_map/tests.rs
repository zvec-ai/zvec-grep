use super::*;
use crate::{
    domain::{Content, FileFormat, Range as EntityRange},
    extraction::{ChunkOptions, markdown, test_source},
};

fn part<L>(text_range: Range<usize>, location: L) -> SourcePart<L> {
    SourcePart {
        text_range,
        location,
    }
}

#[test]
fn text_mapping_preserves_unicode_crlf_and_eof_coordinates() {
    let map = TextSourceMap::new("前\r\n😀x\n");
    assert_eq!(map.line_starts(), [0, 5, 11]);
    for (span, coordinates) in [
        (0..4, (0, 4, 1, 1, 0, 4)),
        (5..10, (5, 10, 2, 2, 0, 5)),
        (5..11, (5, 11, 2, 3, 0, 0)),
        (11..11, (11, 11, 3, 3, 0, 0)),
    ] {
        let (start, end, start_line, end_line, start_column, end_column) = coordinates;
        assert_eq!(
            map.map_range(span).expect("text selection"),
            TextRange::from_coordinates(start, end, start_line, end_line, start_column, end_column)
                .expect("expected coordinates")
        );
    }
    assert_eq!(
        TextSourceMap::new("").map_range(0..0).expect("empty text"),
        TextRange::from_coordinates(0, 0, 1, 1, 0, 0).expect("empty coordinates")
    );
}

fn assert_invalid_selections(map: &impl SourceMap) {
    for (start, end) in [
        (1, 3),
        (0, 1),
        (6, 9),
        (6, 6),
        (0, 12),
        (5, 4),
        (usize::MAX, usize::MAX),
    ] {
        assert!(map.map_range(start..end).is_err(), "{start}..{end}");
    }
}

#[test]
fn both_mappers_reject_invalid_selection_boundaries() {
    let text = "前\r\n😀x\n";
    assert_invalid_selections(&TextSourceMap::new(text));
    let map = SegmentSourceMap::new(text, vec![part(0..text.len(), "block")]).expect("source map");
    assert_invalid_selections(&map);
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BlockLocation {
    document: &'static str,
    block: usize,
}

fn segmented_text() -> SegmentSourceMap<'static, BlockLocation> {
    SegmentSourceMap::new(
        "A😀B\r\n\r\nC中文D",
        vec![
            part(
                0..6,
                BlockLocation {
                    document: "first",
                    block: 2,
                },
            ),
            part(
                10..18,
                BlockLocation {
                    document: "second",
                    block: 7,
                },
            ),
        ],
    )
    .expect("source map")
}

#[test]
fn clips_and_rebases_text_spans_without_changing_source_locations() {
    let map = segmented_text();
    let selected = map.map_range(1..17).expect("selection across blocks");
    assert_eq!(selected.text(), "😀B\r\n\r\nC中文");
    assert_eq!(selected.text_range(), 1..17);
    assert_eq!(
        selected.parts(),
        [
            part(
                0..5,
                BlockLocation {
                    document: "first",
                    block: 2
                }
            ),
            part(
                9..16,
                BlockLocation {
                    document: "second",
                    block: 7
                }
            ),
        ]
    );
    assert_eq!(map.text_range(), 0..18);
    assert_eq!(map.parts()[0].text_range, 0..6);
}

#[test]
fn entity_then_fragment_mapping_equals_direct_selection() {
    let map = segmented_text();
    let boundaries = map
        .text()
        .char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(map.text().len()))
        .collect::<Vec<_>>();
    for (index, &entity_start) in boundaries.iter().enumerate() {
        for &entity_end in &boundaries[index..] {
            let entity = map.map_range(entity_start..entity_end).expect("entity map");
            let local_boundaries = boundaries
                .iter()
                .copied()
                .filter(|&offset| offset >= entity_start && offset <= entity_end)
                .collect::<Vec<_>>();
            for (index, &start) in local_boundaries.iter().enumerate() {
                for &end in &local_boundaries[index..] {
                    assert_eq!(
                        entity
                            .map_range(start - entity_start..end - entity_start)
                            .expect("fragment map"),
                        map.map_range(start..end).expect("direct selection"),
                        "entity {entity_start}..{entity_end}, fragment {start}..{end}"
                    );
                }
            }
        }
    }
}

#[test]
fn keeps_overlapping_origins_in_source_order() {
    let map = SegmentSourceMap::new(
        "abcdef",
        vec![
            part(0..4, "first"),
            part(2..6, "second"),
            part(2..6, "first"),
        ],
    )
    .expect("overlapping source parts");
    assert_eq!(
        map.map_range(3..5).expect("multiple origins").parts(),
        [
            part(0..1, "first"),
            part(0..2, "second"),
            part(0..2, "first"),
        ]
    );
}

#[test]
fn leaves_unmapped_text_and_empty_selection_policy_to_adapters() {
    let map =
        SegmentSourceMap::new("# title", vec![part(2..7, "heading")]).expect("generated prefix");
    let prefix = map.map_range(0..2).expect("unmapped prefix");
    assert_eq!(prefix.text(), "# ");
    assert!(prefix.parts().is_empty());
    for offset in [0, 2, 4, 7] {
        let empty = map.map_range(offset..offset).expect("empty selection");
        assert_eq!(empty.text_range(), offset..offset);
        assert_eq!(empty.text(), "");
        assert!(empty.parts().is_empty());
    }
    let unmapped = SegmentSourceMap::<()>::new("generated", vec![]).expect("unmapped text");
    assert!(
        unmapped
            .map_range(0..9)
            .expect("unmapped selection")
            .parts()
            .is_empty()
    );
    let empty = SegmentSourceMap::<()>::new("", vec![]).expect("empty map");
    assert_eq!(empty.map_range(0..0).expect("empty selection"), empty);
}

#[test]
fn rejects_invalid_source_parts_before_slicing() {
    let text = "前\r\n😀x\n";
    for (start, end) in [
        (1, 3),
        (0, 1),
        (6, 9),
        (0, 12),
        (5, 4),
        (0, 0),
        (usize::MAX, usize::MAX),
    ] {
        assert!(
            SegmentSourceMap::new(text, vec![part(start..end, ())]).is_err(),
            "{start}..{end}"
        );
    }
    assert!(SegmentSourceMap::new(text, vec![part(5..9, ()), part(0..3, ())]).is_err());
}

#[test]
fn distinguishes_identical_text_at_different_document_positions() {
    let map = SegmentSourceMap::new("same same", vec![part(0..9, "block")]).expect("source map");
    let first = map.map_range(0..4).expect("first occurrence");
    let second = map.map_range(5..9).expect("second occurrence");
    assert_eq!(first.text(), second.text());
    assert_eq!(first.parts(), second.parts());
    assert_ne!(first.text_range(), second.text_range());
}

#[test]
fn markdown_entities_can_be_remapped_without_changing_the_extraction_path() {
    // A converted-format adapter can keep calling Markdown extraction and map
    // its text ranges afterward, without passing a source map into the parser.
    for structured in [false, true] {
        let blocks = [
            format!(
                "{}{}\r\n",
                if structured { "# First\r\n" } else { "" },
                "one 😀\r\n".repeat(15)
            ),
            format!(
                "{}{}\r\n",
                if structured { "## Child\r\n" } else { "" },
                "二 body\r\n".repeat(15)
            ),
        ];
        let text = blocks.concat();
        let source = test_source(FileFormat::Markdown, "converted.md", &text);
        let map = SegmentSourceMap::new(
            &text,
            vec![
                part(0..blocks[0].len(), "first block"),
                part(blocks[0].len()..text.len(), "second block"),
            ],
        )
        .expect("source map");
        let entities = markdown::extract(
            &source,
            ChunkOptions {
                max_chunk_chars: Some(48),
                chunk_overlap_chars: Some(5),
            },
        )
        .expect("Markdown extraction");
        assert!(!entities.is_empty());
        if structured {
            assert_eq!(entities.len(), 2);
            assert!(entities.iter().all(|entity| entity.fragments.len() > 1));
        }
        for entity in &entities {
            let EntityRange::Text(range) = &entity.source_range else {
                panic!("Markdown must keep returning text coordinates");
            };
            let Content::Text(content) = &entity.content else {
                panic!("Markdown must keep returning text content");
            };
            let start = range.start_byte_offset();
            let selected = map
                .map_range(start..range.end_byte_offset())
                .expect("entity mapping");
            assert_eq!(selected.text(), content);
            assert!(!selected.parts().is_empty());
            for fragment in &entity.fragments {
                let local = if let EntityRange::Byte(range) = &fragment.range {
                    usize::try_from(range.start_offset()).expect("start")
                        ..usize::try_from(range.end_offset()).expect("end")
                } else {
                    assert!(
                        matches!(&fragment.range, EntityRange::Full),
                        "Markdown fragments must keep entity-relative byte coordinates"
                    );
                    0..content.len()
                };
                assert_eq!(
                    selected.map_range(local.clone()).expect("fragment mapping"),
                    map.map_range(start + local.start..start + local.end)
                        .expect("document mapping")
                );
            }
        }
    }
}
