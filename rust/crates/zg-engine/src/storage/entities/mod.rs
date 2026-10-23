//! Canonical entities, content, metadata, and fragment selectors.
use std::{collections::HashMap, fmt::Write, path::Path};

use zvec_rust::{Collection, CollectionSchema, DataType, Doc, SearchQuery};

use crate::{
    EngineError, EngineResult,
    domain::{Entity, EntityId, EntityMetadata, FileId},
};

use super::zvec::{
    corrupt, doc_key, fetch_map, native, open_collection, scalar, string_field, u32_field,
    write_docs,
};

mod codec;

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
    /// The storage write entry point validates ownership.
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
            .map(|doc| decode_doc(&doc).map(|entity| (entity.id().clone(), entity)))
            .collect()
    }

    /// Read every canonical ID before replacing or deleting a file's entities.
    /// The caller must hold the store lock throughout this read and the update.
    /// Full query batches are split into disjoint indexed ranges so a retrieval
    /// limit cannot silently omit IDs needed to invalidate incoming graph edges.
    pub(super) fn list_ids(&self, file_id: FileId) -> EngineResult<Vec<EntityId>> {
        let mut ranges: Vec<(Option<String>, Option<String>)> = vec![(None, None)];
        let mut result = Vec::new();
        while let Some((lower, upper)) = ranges.pop() {
            let mut filter = format!("file_id = {}", file_id.get());
            if let Some(lower) = &lower {
                let _ = write!(filter, " AND entity_id >= {}", literal(lower));
            }
            if let Some(upper) = &upper {
                let _ = write!(filter, " AND entity_id < {}", literal(upper));
            }
            let mut query = native(SearchQuery::scalar(1024), "create entity ID query")?;
            native(query.set_filter(&filter), "filter entity IDs")?;
            native(
                query.set_output_fields(&["file_id", "entity_id"]),
                "project entity IDs",
            )?;
            native(query.set_include_vector(false), "omit entity vectors")?;
            let docs = native(self.collection.query(&query), "query file entity IDs")?;
            let mut ids = Vec::with_capacity(docs.len());
            for doc in docs {
                if u32_field(&doc, "file_id")? != file_id.get() {
                    return Err(corrupt("entity query returned another file's record"));
                }
                let id = string_field(&doc, "entity_id")?;
                if doc_key(&doc)? != id {
                    return Err(corrupt("entity identity differs from its index fields"));
                }
                if lower.as_ref().is_some_and(|bound| id < *bound)
                    || upper.as_ref().is_some_and(|bound| id >= *bound)
                {
                    return Err(corrupt(
                        "entity query returned an ID outside its requested range",
                    ));
                }
                ids.push(id);
            }
            ids.sort_unstable();
            ids.dedup();
            if ids.len() < 1024 {
                result.extend(ids.into_iter().map(EntityId::from_string));
            } else {
                let pivot = ids[ids.len() / 2].clone();
                ranges.push((Some(pivot.clone()), upper));
                ranges.push((lower, Some(pivot)));
            }
        }
        Ok(result)
    }

    pub(super) fn validate_ownership(
        &self,
        entities: &[Entity],
        file_id: FileId,
    ) -> EngineResult<()> {
        let keys = entities
            .iter()
            .map(|entity| entity.id().as_str().to_owned())
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

    pub(super) fn optimize(&self) -> EngineResult<()> {
        native(self.collection.optimize(), "optimize entities")
    }
}

fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "\\'"))
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
    doc.set_pk(entity.id().as_str());
    native(
        doc.add_u32("file_id", entity.file_id().get()),
        "encode entity file",
    )?;
    native(
        doc.add_string("entity_id", entity.id().as_str()),
        "encode entity identity",
    )?;
    native(
        doc.add_string("payload", &codec::encode_entity(entity)?),
        "encode canonical entity",
    )?;
    if let Some(metadata) = entity.metadata() {
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
    if doc_key(doc)? != entity.id().as_str()
        || u32_field(doc, "file_id")? != entity.file_id().get()
        || string_field(doc, "entity_id")? != entity.id().as_str()
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
