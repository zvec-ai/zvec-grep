use super::*;
use liteparse::stages::{Block, PositionedBlock};

#[test]
fn utf8_block_spans_preserve_duplicate_text_lists_and_tables() {
    let blocks = vec![
        PositionedBlock::unlocated(Block::Heading {
            level: 2,
            text: "恢复 😀".into(),
        }),
        PositionedBlock::unlocated(Block::Paragraph {
            text: "Repeated text 中文".into(),
            bold: false,
            italic: false,
        }),
        PositionedBlock::unlocated(Block::Paragraph {
            text: "Repeated text 中文".into(),
            bold: false,
            italic: false,
        }),
        PositionedBlock::unlocated(Block::ListItem {
            ordered: false,
            marker: "-".into(),
            level: 0,
            text: "等待".into(),
            bold: false,
            italic: false,
        }),
        PositionedBlock::unlocated(Block::ListItem {
            ordered: false,
            marker: "-".into(),
            level: 0,
            text: "重试".into(),
            bold: false,
            italic: false,
        }),
        PositionedBlock::unlocated(Block::Table {
            header: Some(vec!["Name".into(), "Value".into()]),
            rows: vec![vec!["等待".into(), "5".into()]],
        }),
    ];
    let rendered = stages::render_blocks(&blocks);
    let spans = block_spans(&blocks, &rendered).expect("exact spans");
    for (block, span) in blocks.iter().zip(&spans) {
        assert_eq!(
            &rendered[span.clone()],
            stages::render_blocks(std::slice::from_ref(block))
        );
    }
    assert_ne!(spans[1], spans[2]);
    let map = PdfSourceMap {
        markdown_start: 0,
        text_len: rendered.len(),
        parts: spans
            .into_iter()
            .enumerate()
            .map(|(i, range)| PdfSourcePart {
                range,
                block: PdfBlock {
                    page: u32::try_from(i / 2 + 1).expect("page"),
                    id: u32::try_from(i + 1).expect("id"),
                    page_width: 612.0,
                    page_height: 792.0,
                    bbox: None,
                },
            })
            .collect(),
    };
    map.validate(&rendered).expect("complete UTF-8 map");
    let second = &map.parts[2];
    assert_eq!(
        map.locate(second.range.clone())
            .expect("location")
            .page_numbers(),
        vec![2]
    );
}

#[test]
fn raw_boxes_preserve_fractional_and_out_of_page_coordinates() {
    let b = Rect {
        x: -10.125,
        y: 20.0625,
        width: 120.5,
        height: 40.75,
    };
    assert_eq!(
        raw_box(&b),
        PdfRect {
            x: -10.125,
            y: 20.0625,
            width: 120.5,
            height: 40.75
        }
    );
}

#[test]
fn empty_native_pages_do_not_create_placeholder_entities() {
    let page = serde_json::from_value(
        serde_json::json!({"page_number":1,"page_width":612.0,"page_height":792.0,"text_items":[]}),
    )
    .expect("page");
    let source = finish(
        vec![page],
        vec![],
        SourcePath::new("scan.pdf").expect("path"),
        &CancellationToken::new(),
    )
    .expect("empty conversion");
    assert!(source.text.text.is_empty());
    assert!(source.source_map.parts.is_empty());
}

#[test]
fn source_boxes_use_native_page_coordinates_after_projection() {
    use liteparse::types::TextItem;
    let mut page: Page = serde_json::from_value(
        serde_json::json!({"page_number":1,"page_width":612.0,"page_height":792.0,"text_items":[]}),
    )
    .expect("page");
    page.text_items = vec![
        TextItem {
            text: "Native source coordinates remain on the physical page.".into(),
            x: 54.0,
            y: 72.0,
            width: 230.0,
            height: 12.0,
            font_size: Some(12.0),
            font_name: Some("Helvetica".into()),
            ..TextItem::default()
        },
        TextItem {
            text: "A second line continues the source paragraph.".into(),
            x: 54.0,
            y: 90.0,
            width: 230.0,
            height: 12.0,
            font_size: Some(12.0),
            font_name: Some("Helvetica".into()),
            ..TextItem::default()
        },
    ];
    let expected = parser().parse_from_pages(vec![page.clone()], vec![]);
    let source = finish(
        vec![page],
        vec![],
        SourcePath::new("geometry.pdf").expect("path"),
        &CancellationToken::new(),
    )
    .expect("conversion");
    let blocks = expected.pages[0].blocks.as_ref().expect("public blocks");
    assert!(!source.source_map.parts.is_empty());
    for part in &source.source_map.parts {
        let block = &blocks[usize::try_from(part.block.id - 1).expect("id")];
        let expected = block.bbox.as_ref().expect("native box");
        let actual = part.block.bbox.expect("stored box");
        assert_eq!(actual.x.to_bits(), expected.x.to_bits());
        assert_eq!(actual.y.to_bits(), expected.y.to_bits());
        assert_eq!(actual.width.to_bits(), expected.width.to_bits());
        assert_eq!(actual.height.to_bits(), expected.height.to_bits());
        assert_eq!(part.block.page_width.to_bits(), 612.0_f32.to_bits());
        assert_eq!(part.block.page_height.to_bits(), 792.0_f32.to_bits());
        assert!(actual.x >= 50.0 && actual.x + actual.width <= 290.0);
        assert!(actual.y >= 70.0 && actual.y + actual.height <= 105.0);
    }
}
