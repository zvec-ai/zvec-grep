use super::*;
use crate::domain::{CodeMetadata, Content, EntityFragment, FragmentId, Range, SymbolType};

fn entity(file_id: u32, metadata: bool) -> Entity {
    let file_id = FileId::new(file_id);
    let content = Content::Text("canonical content".into());
    let id = EntityId::new(file_id, &content, Range::Full).expect("entity ID");
    Entity::new(
        id.clone(),
        file_id,
        Range::Full,
        content,
        metadata.then(|| {
            EntityMetadata::Code(CodeMetadata {
                symbol_name: Some("symbol".into()),
                symbol_type: Some(SymbolType::Function),
                scope: Some("module".into()),
                signature: Some("fn symbol()".into()),
                visibility: None,
                language: None,
                documentation: Some("documentation".into()),
            })
        }),
        vec![EntityFragment {
            id: FragmentId::new(&id, 0),
            range: Range::Full,
        }],
    )
    .expect("entity")
}

#[test]
fn table_preserves_entities_metadata_and_ownership_across_reopen() {
    super::super::zvec::initialize().expect("native runtime");
    let root = tempfile::tempdir().expect("storage");
    let table = Entities::open(root.path(), false).expect("entities");
    let entities = [entity(1, true), entity(2, false)];
    let ids = entities
        .iter()
        .map(|entity| entity.id().clone())
        .collect::<Vec<_>>();
    table
        .write(&Entities::prepare(&entities).expect("encode entities"))
        .expect("write entities");
    table
        .validate_ownership(&entities[..1], entities[0].file_id())
        .expect("same owner");
    let foreign = Entity::new(
        entities[0].id().clone(),
        FileId::new(99),
        entities[0].source_range(),
        entities[0].content().clone(),
        entities[0].metadata().cloned(),
        entities[0].fragments().to_vec(),
    )
    .expect("foreign entity");
    assert_eq!(
        table
            .validate_ownership(&[foreign], FileId::new(99))
            .expect_err("cannot replace another file's entity")
            .code(),
        EngineError::INVALID_ARGUMENT,
    );
    table.flush().expect("persist entities");
    drop(table);

    let table = Entities::open(root.path(), false).expect("reopen entities");
    let loaded = table.fetch(&ids).expect("load canonical entities");
    assert_eq!(loaded.len(), 2);
    for entity in &entities {
        assert_eq!(&loaded[entity.id()], entity);
    }
    table
        .delete_file(entities[0].file_id())
        .expect("delete one file");
    table
        .delete_file(entities[0].file_id())
        .expect("idempotent deletion");
    assert_eq!(
        table.fetch(&ids).expect("remaining entity"),
        HashMap::from([(entities[1].id().clone(), entities[1].clone()),])
    );
}

#[test]
fn canonical_columns_reject_mismatched_identity_and_corrupt_metadata() {
    super::super::zvec::initialize().expect("native runtime");
    let entity = entity(1, true);
    for field in ["primary_key", "file_id", "entity_id"] {
        let mut doc = encode_doc(&entity).expect("valid entity");
        match field {
            "primary_key" => doc.set_pk("different"),
            "file_id" => doc.add_u32(field, 99).expect("foreign owner"),
            _ => doc
                .add_string(field, "different")
                .expect("foreign identity"),
        }
        assert_eq!(
            decode_doc(&doc)
                .expect_err("inconsistent indexed identity")
                .message(),
            "entity identity differs from its index fields",
        );
    }
    let mut doc = encode_doc(&entity).expect("valid entity");
    doc.add_string("metadata", "not JSON")
        .expect("corrupt metadata");
    let error = decode_doc(&doc).expect_err("metadata corruption is not skipped");
    assert_eq!(error.code(), EngineError::STORAGE_FAILURE);
    assert!(error.message().contains("invalid entity metadata"));
}

fn indexed_entity(id: &str, file_id: u32) -> Doc {
    let mut entity = entity(file_id, false);
    let id = EntityId::from_string(id.to_owned());
    let fragments = vec![EntityFragment {
        id: FragmentId::new(&id, 0),
        range: entity.fragments()[0].range,
    }];
    entity = Entity::new(
        id,
        entity.file_id(),
        entity.source_range(),
        entity.content().clone(),
        entity.metadata().cloned(),
        fragments,
    )
    .expect("fixture identity");
    encode_doc(&entity).expect("canonical entity")
}

#[test]
fn file_ids_are_complete_isolated_and_preserved_across_reopen() {
    super::super::zvec::initialize().expect("native runtime");
    let root = tempfile::tempdir().expect("storage");
    let table = Entities::open(root.path(), false).expect("entities");
    let ids: Vec<_> = (0..2301)
        .map(|ordinal| format!("00000001{ordinal:024x}"))
        .collect();
    table
        .write(
            &ids.iter()
                .map(|id| indexed_entity(id.as_str(), 1))
                .collect::<Vec<_>>(),
        )
        .expect("write several query batches");
    table
        .write(&[indexed_entity("other-file", 2)])
        .expect("write another file");

    assert_eq!(
        table.list_ids_for_file(FileId::new(1)).expect("all IDs"),
        ids
    );
    assert!(
        table
            .list_ids_for_file(FileId::new(99))
            .expect("missing file")
            .is_empty()
    );
    table.flush().expect("persist entities");
    drop(table);

    let table = Entities::open(root.path(), false).expect("reopen entities");
    assert_eq!(
        table
            .list_ids_for_file(FileId::new(1))
            .expect("persisted IDs"),
        ids
    );
    table.delete_file(FileId::new(1)).expect("delete file");
    assert!(
        table
            .list_ids_for_file(FileId::new(1))
            .expect("deleted file")
            .is_empty()
    );
    assert_eq!(
        table
            .list_ids_for_file(FileId::new(2))
            .expect("other file retained"),
        vec!["other-file".to_owned()]
    );
}

#[test]
fn full_file_id_batch_handles_escaped_range_bounds() {
    super::super::zvec::initialize().expect("native runtime");
    let root = tempfile::tempdir().expect("storage");
    let table = Entities::open(root.path(), false).expect("entities");
    let ids: Vec<_> = (0..1024)
        .map(|ordinal| format!("quoted'\\id-{ordinal:04}"))
        .collect();
    table
        .write(
            &ids.iter()
                .map(|id| indexed_entity(id.as_str(), 7))
                .collect::<Vec<_>>(),
        )
        .expect("write a full query batch");
    assert_eq!(
        table
            .list_ids_for_file(FileId::new(7))
            .expect("escaped filter bounds"),
        ids
    );
}

#[test]
fn file_id_listing_rejects_inconsistent_indexed_identity() {
    super::super::zvec::initialize().expect("native runtime");
    let root = tempfile::tempdir().expect("storage");
    let table = Entities::open(root.path(), false).expect("entities");
    let mut doc = indexed_entity("entity", 1);
    doc.add_string("entity_id", "different")
        .expect("corrupt indexed identity");
    table.write(&[doc]).expect("write corrupt record");
    let error = table
        .list_ids_for_file(FileId::new(1))
        .expect_err("corruption must not silently drop an old ID");
    assert_eq!(error.code(), EngineError::STORAGE_FAILURE);
    assert_eq!(
        error.message(),
        "entity identity differs from its index fields"
    );
}
