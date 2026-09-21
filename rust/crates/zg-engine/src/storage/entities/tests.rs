use super::*;
use crate::domain::{CodeMetadata, Content, EntityFragment, FragmentId, Range, SymbolType};

fn entity(file_id: u32, metadata: bool) -> Entity {
    let file_id = FileId::new(file_id);
    let content = Content::Text("canonical content".into());
    let id = EntityId::new(file_id, &content, Range::Full).expect("entity ID");
    Entity {
        fragments: vec![EntityFragment {
            id: FragmentId::new(&id, 0),
            range: Range::Full,
        }],
        id,
        file_id,
        source_range: Range::Full,
        content,
        metadata: metadata.then(|| {
            EntityMetadata::Code(CodeMetadata {
                symbol_name: Some("symbol".into()),
                symbol_type: Some(SymbolType::Function),
                scope: Some("module".into()),
                signature: Some("fn symbol()".into()),
                documentation: Some("documentation".into()),
            })
        }),
    }
}

#[test]
fn table_preserves_entities_metadata_and_ownership_across_reopen() {
    super::super::zvec::initialize().expect("native runtime");
    let root = tempfile::tempdir().expect("storage");
    let table = Entities::open(root.path(), false).expect("entities");
    let entities = [entity(1, true), entity(2, false)];
    let ids = entities
        .iter()
        .map(|entity| entity.id.clone())
        .collect::<Vec<_>>();
    table
        .write(&Entities::prepare(&entities).expect("encode entities"))
        .expect("write entities");
    table
        .validate_ownership(&entities[..1], entities[0].file_id)
        .expect("same owner");
    let mut foreign = entities[0].clone();
    foreign.file_id = FileId::new(99);
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
        assert_eq!(&loaded[&entity.id], entity);
    }
    table
        .delete_file(entities[0].file_id)
        .expect("delete one file");
    table
        .delete_file(entities[0].file_id)
        .expect("idempotent deletion");
    assert_eq!(
        table.fetch(&ids).expect("remaining entity"),
        HashMap::from([(entities[1].id.clone(), entities[1].clone()),])
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
