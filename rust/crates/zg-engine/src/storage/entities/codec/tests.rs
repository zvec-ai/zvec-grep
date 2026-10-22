use super::*;
use crate::domain::{CodeMetadata, MarkdownMetadata, SymbolType};
use serde_json::{Value, json};

fn text_range() -> Range {
    Range::Text(TextRange::from_coordinates(7, 18, 2, 2, 0, 11).expect("text range"))
}

fn entity() -> Entity {
    let content =
        Content::Image(ImageContent::new(vec![0, 1, 255], FileFormat::Png).expect("image"));
    make_entity(FileId::new(1), content, text_range(), None, &[Range::Full])
}

fn make_entity(
    file_id: FileId,
    content: Content,
    source_range: Range,
    metadata: Option<EntityMetadata>,
    ranges: &[Range],
) -> Entity {
    let id = EntityId::new(file_id, &content, source_range).expect("entity id");
    let fragments = ranges
        .iter()
        .enumerate()
        .map(|(ordinal, &range)| EntityFragment {
            id: FragmentId::new(&id, u32::try_from(ordinal).expect("fragment ordinal")),
            range,
        })
        .collect();
    Entity::new(id, file_id, source_range, content, metadata, fragments).expect("entity")
}

fn round_trip(entity: &Entity) {
    let encoded = encode_entity(entity).expect("encode entity");
    assert_eq!(
        decode_entity(&encoded, entity.metadata()).expect("decode entity"),
        *entity
    );
}

#[test]
fn code_records_preserve_content_and_validate_utf8_fragment_ranges() {
    let content = Content::Code("a中文b".into());
    let file_id = FileId::new(42);
    let code = make_entity(
        file_id,
        content,
        Range::Full,
        None,
        &[Range::Byte(ByteRange::new(1, 7).expect("UTF-8 range"))],
    );
    round_trip(&code);
    let mut encoded: Value =
        serde_json::from_str(&encode_entity(&code).expect("write code")).expect("JSON");
    assert_eq!(encoded["content"], json!({"kind":"code", "value":"a中文b"}));
    encoded["fragments"][0]["range"]["end_offset"] = json!(6);
    assert_corrupt_entity(&encoded);
}

fn assert_corrupt_entity(record: &Value) {
    let error = decode_entity(&record.to_string(), None).expect_err("invalid stored entity");
    assert_eq!(error.code(), EngineError::STORAGE_FAILURE, "{error}");
}

#[test]
fn source_identities_round_trip_the_full_u32_range() {
    for id in [0, u32::MAX] {
        let entity = make_entity(
            FileId::new(id),
            entity().content().clone(),
            text_range(),
            None,
            &[Range::Full],
        );
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
fn entity_records_preserve_image_content_ranges_and_external_metadata() {
    round_trip(&entity());
    let record: Value =
        serde_json::from_str(&encode_entity(&entity()).expect("encode entity")).expect("JSON");
    assert_eq!(record["content"]["value"]["data"], "AAH/");
    assert_eq!(record["content"]["value"]["format"], "png");
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
        let metadata = Some(EntityMetadata::Code(CodeMetadata {
            symbol_type: Some(SymbolType::Function),
            symbol_name: Some("symbol".into()),
            scope: Some("module".into()),
            signature: Some("pub async fn symbol()".into()),
            documentation: Some("documentation".into()),
            language: None,
            visibility: None,
        }));
        let entity = make_entity(
            FileId::new(1),
            entity().content().clone(),
            range,
            metadata,
            &[Range::Full],
        );
        round_trip(&entity);
        let record: Value =
            serde_json::from_str(&encode_entity(&entity).expect("encode entity")).expect("JSON");
        assert_eq!(record["source_range"], stored_range);
    }
}

#[test]
fn fragment_selectors_preserve_utf8_slices_without_repeating_content_or_ownership() {
    let source_range =
        Range::Text(TextRange::from_coordinates(10, 18, 1, 1, 10, 18).expect("range"));
    let metadata = Some(EntityMetadata::Markdown(MarkdownMetadata {
        heading: Some("heading".into()),
        level: Some(2),
        scope: Some("parent".into()),
    }));
    let entity = make_entity(
        FileId::new(1),
        Content::Text("a中文b".into()),
        source_range,
        metadata,
        &[
            Range::Full,
            Range::Byte(ByteRange::new(1, 7).expect("ordered byte offsets")),
        ],
    );
    round_trip(&entity);
    for source_range in [
        Range::Full,
        Range::Byte(ByteRange::new(10, 12).expect("source bytes")),
        Range::Text(TextRange::from_coordinates(10, 12, 1, 1, 10, 12).expect("source text")),
    ] {
        let mut fragments = entity.fragments().to_vec();
        fragments.reverse();
        let transformed = Entity::new(
            entity.id().clone(),
            entity.file_id(),
            source_range,
            entity.content().clone(),
            entity.metadata().cloned(),
            fragments,
        )
        .expect("entity with reversed fragments");
        round_trip(&transformed);
    }
    let encoded = encode_entity(&entity).expect("encode entity");
    let record: Value = serde_json::from_str(&encoded).expect("JSON");
    assert_eq!(
        record["fragments"][1],
        json!({
            "id":entity.fragments()[1].id.as_str(), "range":{"kind":"byte", "start_offset":1, "end_offset":7},
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
fn code_records_preserve_kind_without_metadata_and_validate_text_ranges() {
    let entity = make_entity(
        FileId::new(1),
        Content::Code("a中文b".into()),
        Range::Full,
        None,
        &[Range::Byte(ByteRange::new(1, 7).expect("UTF-8 slice"))],
    );
    round_trip(&entity);
    let record: Value =
        serde_json::from_str(&encode_entity(&entity).expect("encode code")).expect("JSON");
    assert_eq!(record["content"], json!({"kind":"code", "value":"a中文b"}));
    assert!(record.get("metadata").is_none());
    assert_eq!(entity.content().kind(), crate::domain::ContentKind::Code);
    assert_ne!(
        entity.id(),
        &EntityId::new(
            entity.file_id(),
            &Content::Text("a中文b".into()),
            Range::Full
        )
        .expect("text entity id"),
    );
    for (start, end) in [(2, 7), (1, 9), (1, 1)] {
        let mut invalid = record.clone();
        invalid["fragments"][0]["range"] = json!({
            "kind":"byte", "start_offset":start, "end_offset":end,
        });
        assert_corrupt_entity(&invalid);
    }
    let mut invalid = record;
    invalid["content"]["value"] = json!(" \n\t");
    invalid["fragments"][0]["range"] = json!({"kind":"full"});
    assert_corrupt_entity(&invalid);
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
        record["content"]["value"][field] = value;
        assert_corrupt_entity(&record);
    }
    for (field, value) in [("start_line", 0), ("end_byte_column", 12)] {
        let mut record = original.clone();
        record["source_range"][field] = json!(value);
        assert_corrupt_entity(&record);
    }
    assert!(decode_entity("not JSON", None).is_err());
}
