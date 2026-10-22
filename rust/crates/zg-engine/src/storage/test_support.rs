//! Common storage fixtures; no production storage behavior lives here.
use super::{IndexStore, types::WorkspaceIndexStorageOptions};
use crate::domain::{EntityId, FileGraphNode, SymbolType, model::EmbeddingModelInfo};
use std::path::Path;

pub(crate) fn node(id: &str) -> FileGraphNode {
    FileGraphNode {
        id: id.to_owned(),
        kind: SymbolType::Function,
        name: Some(id.to_owned()),
        qualified_name: id.to_owned(),
        language: "rust".into(),
        start_line: 1,
        end_line: 1,
        start_column: 0,
        end_column: 1,
        signature: None,
        doc: None,
        arity: None,
        visibility: None,
        is_exported: false,
    }
}

pub(crate) fn open_store(
    path: &Path,
    read_only: bool,
    embedding: EmbeddingModelInfo,
) -> IndexStore {
    let options = if read_only {
        WorkspaceIndexStorageOptions::ReadOnly {
            storage_path: path.to_owned(),
        }
    } else {
        WorkspaceIndexStorageOptions::ReadWrite {
            storage_path: path.to_owned(),
            embeddings: vec![embedding],
        }
    };
    IndexStore::open(options).expect("open test storage")
}

pub(crate) fn corrupt_entity_bodies(home: &Path, ids: &[EntityId]) {
    corrupt_entity_field(home, ids, "payload");
}

pub(crate) fn corrupt_entity_metadata(home: &Path, ids: &[EntityId]) {
    corrupt_entity_field(home, ids, "metadata");
}

pub(crate) fn corrupt_entity_identity(home: &Path, ids: &[EntityId]) {
    corrupt_entity_field(home, ids, "entity_id");
}

fn corrupt_entity_field(home: &Path, ids: &[EntityId], field: &str) {
    let path = home.join("storage/entities");
    let entities =
        zvec_rust::Collection::open(super::zvec::native_path(&path).expect("path"), None)
            .expect("entities");
    for id in ids {
        let mut doc = entities.fetch(&[id.as_str()]).expect("entity").remove(0);
        doc.add_string(field, "not JSON").expect("corrupt field");
        assert_eq!(entities.upsert(&[&doc]).expect("write body").error_count, 0);
    }
    entities.flush().expect("persist bodies");
}
