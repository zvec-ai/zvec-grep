//! Canonical entities, content, metadata, and fragment selectors.
use std::{collections::HashMap, path::Path};

use zvec_rust::{Collection, CollectionSchema, DataType, Doc};

use crate::{
    EngineError, EngineResult,
    domain::{Entity, EntityId, EntityMetadata, FileId},
};

use super::zvec::{
    corrupt, doc_key, fetch_map, native, open_collection, scalar, string_field, u32_field,
    write_docs,
};

mod codec;
pub(super) use codec::validate_content;

pub(super) struct Entities {
    collection: Collection,
}

impl Entities {
    #[cfg(test)]
    pub(super) fn collection(&self) -> &Collection {
        &self.collection
    }

    pub(super) fn open(path: &Path, read_only: bool) -> EngineResult<Self> {
        Ok(Self {
            collection: open_collection(&path.join("entities"), &schema()?, read_only)?,
        })
    }

    /// Encode the full replacement before any existing records are removed.
    /// The storage write entry point validates domain content and ownership.
    pub(super) fn prepare(entities: &[Entity]) -> EngineResult<Vec<Doc>> {
        entities.iter().map(encode_doc).collect()
    }

    pub(super) fn write(&self, docs: &[Doc]) -> EngineResult<()> {
        write_docs(
            &self.collection,
            docs,
            "write entities and canonical fragments",
        )
    }

    pub(super) fn fetch(&self, ids: &[EntityId]) -> EngineResult<HashMap<EntityId, Entity>> {
        let keys = ids
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect::<Vec<_>>();
        fetch_map(&self.collection, &keys)?
            .into_values()
            .map(|doc| decode_doc(&doc).map(|entity| (entity.id.clone(), entity)))
            .collect()
    }

    pub(super) fn validate_ownership(
        &self,
        entities: &[Entity],
        file_id: FileId,
    ) -> EngineResult<()> {
        let keys = entities
            .iter()
            .map(|entity| entity.id.as_str().to_owned())
            .collect::<Vec<_>>();
        for doc in fetch_map(&self.collection, &keys)?.into_values() {
            if u32_field(&doc, "file_id")? != file_id.get() {
                return Err(EngineError::invalid_argument(
                    "entity or fragment ID is already owned by another file",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn delete_file(&self, file_id: FileId) -> EngineResult<()> {
        native(
            self.collection
                .delete_by_filter(&format!("file_id = {}", file_id.get())),
            "delete source entities",
        )
    }

    pub(super) fn flush(&self) -> EngineResult<()> {
        native(self.collection.flush(), "flush entities")
    }
}

fn schema() -> EngineResult<CollectionSchema> {
    let mut schema = native(CollectionSchema::new("entities"), "create entities schema")?;
    scalar(&mut schema, "file_id", DataType::Uint32, false, true)?;
    scalar(&mut schema, "entity_id", DataType::String, false, true)?;
    scalar(&mut schema, "payload", DataType::String, false, false)?;
    scalar(&mut schema, "metadata", DataType::String, true, false)?;
    Ok(schema)
}

fn encode_doc(entity: &Entity) -> EngineResult<Doc> {
    let mut doc = native(Doc::new(), "create canonical entity record")?;
    doc.set_pk(entity.id.as_str());
    native(
        doc.add_u32("file_id", entity.file_id.get()),
        "encode entity file",
    )?;
    native(
        doc.add_string("entity_id", entity.id.as_str()),
        "encode entity identity",
    )?;
    native(
        doc.add_string("payload", &codec::encode_entity(entity)?),
        "encode canonical entity",
    )?;
    if let Some(metadata) = &entity.metadata {
        let value = serde_json::to_value(metadata)
            .map_err(|error| corrupt(format!("encode entity metadata: {error}")))?;
        let json = serde_json::to_string(&value)
            .map_err(|error| corrupt(format!("encode entity metadata: {error}")))?;
        native(doc.add_string("metadata", &json), "encode entity metadata")?;
    }
    Ok(doc)
}

fn decode_doc(doc: &Doc) -> EngineResult<Entity> {
    let metadata = decode_metadata(doc)?;
    let entity = codec::decode_entity(&string_field(doc, "payload")?, metadata.as_ref())?;
    if doc_key(doc)? != entity.id.as_str()
        || u32_field(doc, "file_id")? != entity.file_id.get()
        || string_field(doc, "entity_id")? != entity.id.as_str()
    {
        return Err(corrupt("entity identity differs from its index fields"));
    }
    Ok(entity)
}

fn decode_metadata(doc: &Doc) -> EngineResult<Option<EntityMetadata>> {
    if !doc.has_field("metadata") || doc.is_field_null("metadata") {
        return Ok(None);
    }
    let json = string_field(doc, "metadata")?;
    serde_json::from_str(&json)
        .map(Some)
        .map_err(|error| corrupt(format!("invalid entity metadata: {error}")))
}

#[cfg(test)]
mod tests;
