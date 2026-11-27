use super::*;

fn map() -> PdfSourceMap {
    let block = |page, id, bbox| PdfBlock {
        page,
        id,
        page_width: if page == 4 { 612.0 } else { 595.0 },
        page_height: if page == 4 { 792.0 } else { 842.0 },
        bbox,
    };
    let rect = PdfRect {
        x: 10.25,
        y: 20.5,
        width: 100.0,
        height: 15.0,
    };
    PdfSourceMap {
        markdown_start: 100,
        text_len: 9,
        parts: vec![
            PdfSourcePart {
                range: 0..2,
                block: block(4, 1, Some(rect)),
            },
            PdfSourcePart {
                range: 3..4,
                block: block(4, 1, Some(rect)),
            },
            PdfSourcePart {
                range: 5..7,
                block: block(5, 2, Some(rect)),
            },
            PdfSourcePart {
                range: 8..9,
                block: block(5, 3, None),
            },
        ],
    }
}

#[test]
fn locates_cross_page_fragments_without_carrying_text_mapping() {
    let source = map();
    source.validate("ab c de f").expect("valid map");
    let whole = source.locate(0..9).expect("whole entity");
    assert_eq!(whole.page_numbers(), [4, 5]);
    assert_eq!(
        whole.pages[0].rects.len(),
        1,
        "repeated source block is deduplicated"
    );
    assert_eq!(whole.pages[1].page_width.to_bits(), 595.0_f32.to_bits());
    let fragment = source.locate(5..7).expect("page five fragment");
    assert_eq!(fragment.page_numbers(), [5]);
    assert_eq!(fragment.pages[0].rects, whole.pages[1].rects);
    let page_only = source.locate(8..9).expect("block without geometry");
    assert_eq!(page_only.page_numbers(), [5]);
    assert!(page_only.pages[0].rects.is_empty());
    let encoded = serde_json::to_value(&fragment).expect("location JSON");
    assert_eq!(encoded.as_object().expect("object").len(), 1);
    assert!(encoded.get("pages").is_some());
    assert!(encoded["pages"][0].get("block").is_none());
}

#[test]
fn locations_keep_separate_regions_and_known_pages() {
    let mut source = map();
    source.parts[1].block.id = 2;
    let location = source.locate(0..9).expect("multiple blocks");
    assert_eq!(
        location.pages[0].rects.len(),
        2,
        "distinct blocks retain distinct regions"
    );
    assert_eq!(source.locate(4..5).expect("separator").page_numbers(), [4]);
    assert_eq!(
        source.locate(5..5).expect("empty match").page_numbers(),
        [5]
    );
    assert_eq!(source.locate(9..9).expect("end match").page_numbers(), [5]);
    assert!(source.locate(10..10).is_err());
    assert!(source.locate(std::ops::Range { start: 8, end: 2 }).is_err());
}

#[test]
fn physical_location_validation_rejects_invalid_pages_and_geometry() {
    let original = map().locate(0..9).expect("location");
    for case in 0..5 {
        let mut location = original.clone();
        match case {
            0 => location.pages.clear(),
            1 => location.pages[0].page = 0,
            2 => location.pages[1].page = 4,
            3 => location.pages[0].page_width = f32::NAN,
            _ => location.pages[0].rects[0].width = -1.0,
        }
        assert!(location.validate().is_err(), "case {case}");
    }
}

#[test]
fn identical_text_in_the_same_block_keeps_distinct_entity_ids() {
    use crate::domain::{Content, EntityId, FileId, Range as SourceRange};
    let source = map();
    let content = Content::Text("ab c de f".into());
    let range = SourceRange::Pdf(source.locate(0..source.text_len).expect("location").into());
    let id =
        EntityId::new_with_source_map(FileId::new(1), &content, &range, Some(&source)).expect("id");
    let mut other = source.clone();
    other.markdown_start += source.text_len;
    let other_id = EntityId::new_with_source_map(FileId::new(1), &content, &range, Some(&other))
        .expect("other id");
    assert_ne!(id, other_id);
    assert_eq!(
        id,
        EntityId::new_with_source_map(FileId::new(1), &content, &range, Some(&source))
            .expect("deterministic id")
    );
}
