use super::*;
use crate::domain::{CodeMetadata, MarkdownMetadata, SymbolType};
use serde_json::{Value, json};

fn text_range() -> Range {
    Range::Text(TextRange::from_coordinates(7, 18, 2, 2, 0, 11).expect("text range"))
}

fn cell(
    row: usize,
    column: usize,
    row_span: usize,
    column_span: usize,
    kind: TableCellRole,
) -> TableCell {
    TableCell {
        row,
        column,
        row_span,
        column_span,
        contents: vec![Content::Text(format!("{row},{column}"))],
        kind,
    }
}

fn entity() -> Entity {
    let image = Content::Image(ImageContent::new(vec![0, 1, 255], FileFormat::Png).expect("image"));
    let mut nested = cell(1, 1, 1, 1, TableCellRole::Unknown);
    nested.contents.push(Content::Table(TableContent {
        row_count: 1,
        column_count: 1,
        cells: vec![cell(0, 0, 1, 1, TableCellRole::Header)],
    }));
    let mut mixed = cell(0, 1, 1, 2, TableCellRole::Data);
    mixed.contents.push(image);
    let content = Content::Table(TableContent {
        row_count: 2,
        column_count: 3,
        cells: vec![
            cell(0, 0, 2, 1, TableCellRole::Header),
            mixed,
            nested,
            cell(1, 2, 1, 1, TableCellRole::Data),
        ],
    });
    let id = EntityId::new(FileId::new(1), &content, text_range()).expect("entity id");
    Entity {
        id: id.clone(),
        file_id: FileId::new(1),
        source_range: text_range(),
        content,
        metadata: None,
        fragments: vec![EntityFragment {
            id: FragmentId::new(&id, 0),
            range: Range::Full,
        }],
    }
}

fn round_trip(entity: &Entity) {
    let encoded = encode_entity(entity).expect("encode entity");
    assert_eq!(
        decode_entity(&encoded, entity.metadata.as_ref()).expect("decode entity"),
        *entity
    );
}

fn assert_corrupt_entity(record: &Value) {
    let error = decode_entity(&record.to_string(), None).expect_err("invalid stored entity");
    assert_eq!(error.code(), EngineError::STORAGE_FAILURE, "{error}");
}

#[test]
fn source_identities_round_trip_the_full_u32_range() {
    for id in [0, u32::MAX] {
        let mut entity = entity();
        entity.file_id = FileId::new(id);
        round_trip(&entity);
    }
}

#[test]
fn stored_file_identities_reject_values_outside_u32() {
    let mut record: Value =
        serde_json::from_str(&encode_entity(&entity()).expect("entity")).expect("JSON");
    for invalid in [json!(-1), json!(u64::from(u32::MAX) + 1), json!(1e20)] {
        record["file_id"] = invalid;
        assert_corrupt_entity(&record);
    }
}

#[test]
fn entity_records_preserve_structured_content_ranges_and_external_metadata() {
    round_trip(&entity());
    let record: Value =
        serde_json::from_str(&encode_entity(&entity()).expect("encode entity")).expect("JSON");
    assert_eq!(
        record["content"]["value"]["cells"][1]["contents"][1]["value"]["data"],
        "AAH/"
    );
    assert_eq!(
        record["content"]["value"]["cells"][1]["contents"][1]["value"]["format"],
        "png"
    );
    assert!(
        record.get("metadata").is_none(),
        "metadata is supplied by its dedicated storage column"
    );
    for (range, stored_range) in [
        (Range::Full, json!({"kind":"full"})),
        (
            text_range(),
            json!({"kind":"text", "start_line":2, "end_line":2, "start_byte_offset":7, "end_byte_offset":18, "start_byte_column":0, "end_byte_column":11}),
        ),
        (
            Range::Byte(ByteRange::new(2, u64::MAX).expect("ordered byte offsets")),
            json!({"kind":"byte", "start_offset":2, "end_offset":u64::MAX}),
        ),
    ] {
        let mut entity = entity();
        entity.source_range = range;
        entity.metadata = Some(EntityMetadata::Code(CodeMetadata {
            symbol_type: Some(SymbolType::Function),
            symbol_name: Some("symbol".into()),
            scope: Some("module".into()),
            signature: Some("pub async fn symbol()".into()),
            documentation: Some("documentation".into()),
        }));
        round_trip(&entity);
        let record: Value =
            serde_json::from_str(&encode_entity(&entity).expect("encode entity")).expect("JSON");
        assert_eq!(record["source_range"], stored_range);
    }
}

#[test]
fn fragment_selectors_preserve_utf8_slices_without_repeating_content_or_ownership() {
    let mut entity = entity();
    entity.content = Content::Text("a中文b".into());
    entity.source_range =
        Range::Text(TextRange::from_coordinates(10, 18, 1, 1, 10, 18).expect("range"));
    entity.fragments.push(EntityFragment {
        id: FragmentId::new(&entity.id, 1),
        range: Range::Byte(ByteRange::new(1, 7).expect("ordered byte offsets")),
    });
    entity.metadata = Some(EntityMetadata::Markdown(MarkdownMetadata {
        heading: Some("heading".into()),
        level: Some(2),
        scope: Some("parent".into()),
    }));
    round_trip(&entity);
    for source_range in [
        Range::Full,
        Range::Byte(ByteRange::new(10, 12).expect("source bytes")),
        Range::Text(TextRange::from_coordinates(10, 12, 1, 1, 10, 12).expect("source text")),
    ] {
        let mut transformed = entity.clone();
        transformed.source_range = source_range;
        transformed.fragments.reverse();
        round_trip(&transformed);
    }
    let encoded = encode_entity(&entity).expect("encode entity");
    let record: Value = serde_json::from_str(&encoded).expect("JSON");
    assert_eq!(
        record["fragments"][1],
        json!({
            "id":entity.fragments[1].id.as_str(), "range":{"kind":"byte", "start_offset":1, "end_offset":7},
        })
    );
    for (start, end) in [(2, 7), (1, 9), (1, 1), (7, 1)] {
        let mut invalid = record.clone();
        invalid["fragments"][1]["range"] = json!({
            "kind":"byte", "start_offset":start, "end_offset":end,
        });
        assert_corrupt_entity(&invalid);
    }
    let mut invalid = record.clone();
    invalid["fragments"][1]["range"] = json!({
        "kind":"text", "start_line":1, "end_line":1, "start_byte_offset":1,
        "end_byte_offset":7, "start_byte_column":1, "end_byte_column":7,
    });
    assert_corrupt_entity(&invalid);
    let mut invalid = record.clone();
    invalid["fragments"][1]["range"]["start_line"] = json!(1);
    assert_corrupt_entity(&invalid);
    let mut invalid = record.clone();
    invalid["fragments"][1]["content_range"] = json!({"kind":"full"});
    assert_corrupt_entity(&invalid);
    let mut invalid = record.clone();
    invalid["range"] = invalid["source_range"].clone();
    assert_corrupt_entity(&invalid);
    let mut invalid = record;
    invalid["fragments"] = json!([]);
    assert_corrupt_entity(&invalid);
}

#[test]
fn rejects_invalid_table_geometry_without_allocating_a_dense_grid() {
    let original: Value =
        serde_json::from_str(&encode_entity(&entity()).expect("encode entity")).expect("JSON");
    for (field, value) in [
        ("row_span", 0),
        ("row_span", 3),
        ("column_span", 2),
        ("column", usize::MAX),
    ] {
        let mut record = original.clone();
        record["content"]["value"]["cells"][0][field] = json!(value);
        assert_corrupt_entity(&record);
    }
    let mut table = TableContent {
        row_count: usize::MAX,
        column_count: usize::MAX,
        cells: vec![
            cell(0, 0, 1, 1, TableCellRole::Unknown),
            cell(usize::MAX - 1, usize::MAX - 1, 1, 1, TableCellRole::Data),
        ],
    };
    validate_table(&table).expect("sparse table");
    table.cells.reverse();
    assert!(validate_table(&table).is_err());
    table.cells = vec![
        cell(0, 0, 2, 2, TableCellRole::Data),
        cell(1, 1, 1, 1, TableCellRole::Data),
    ];
    assert!(validate_table(&table).is_err());
    let mut entity = entity();
    let mut content = Content::Text("nested".into());
    for depth in 1..=MAX_TABLE_DEPTH + 1 {
        let mut nested = cell(0, 0, 1, 1, TableCellRole::Data);
        nested.contents = vec![content];
        content = Content::Table(TableContent {
            row_count: 1,
            column_count: 1,
            cells: vec![nested],
        });
        entity.content = content.clone();
        if depth <= MAX_TABLE_DEPTH {
            round_trip(&entity);
        } else {
            assert!(validate_content(&entity.content).is_err());
            let encoded = encode_entity(&entity).expect("encode invalid fixture");
            assert!(decode_entity(&encoded, None).is_err());
        }
    }
}

#[test]
fn rejects_corrupt_identities_images_and_ranges() {
    let original: Value =
        serde_json::from_str(&encode_entity(&entity()).expect("encode entity")).expect("JSON");
    let mut record = original.clone();
    record["file_id"] = json!("");
    assert_corrupt_entity(&record);
    for (field, value) in [
        ("format", json!("rust")),
        ("format", json!("not-a-format")),
        ("format", json!(65535)),
        ("data", json!("")),
        ("data", json!("invalid base64!")),
    ] {
        let mut record = original.clone();
        record["content"]["value"]["cells"][1]["contents"][1]["value"][field] = value;
        assert_corrupt_entity(&record);
    }
    for (field, value) in [("start_line", 0), ("end_byte_column", 12)] {
        let mut record = original.clone();
        record["source_range"][field] = json!(value);
        assert_corrupt_entity(&record);
    }
    assert!(decode_entity("not JSON", None).is_err());
}
