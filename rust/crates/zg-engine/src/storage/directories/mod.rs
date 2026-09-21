//! Directory identities and their parent relationships.

use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
};
use zvec_rust::{Collection, CollectionSchema, DataType, Doc};

use super::{
    path::{decode_path, encode_path},
    zvec::{
        corrupt, doc_key, native, open_collection, scalar, string_field, u32_field, write_docs,
    },
};
use crate::{
    EngineError, EngineResult,
    domain::{DirectoryId, DirectoryRecord, SourcePath},
};

mod ids;
use ids::DirectoryIds;

pub(super) struct Directories {
    collection: Collection,
    ids: Mutex<Option<DirectoryIds>>,
    read_only: bool,
}

impl Directories {
    pub(super) fn open(root: &Path, read_only: bool) -> EngineResult<Self> {
        Ok(Self {
            collection: open_collection(
                &root.join("directories"),
                &directories_schema()?,
                read_only,
            )?,
            ids: Mutex::new(None),
            read_only,
        })
    }

    #[cfg(test)]
    pub(super) fn collection(&self) -> &Collection {
        &self.collection
    }

    pub(super) fn load(&self) -> EngineResult<()> {
        drop(self.loaded_ids()?);
        Ok(())
    }

    pub(super) fn get(&self, path: &SourcePath) -> EngineResult<Option<DirectoryId>> {
        Ok(self
            .loaded_ids()?
            .as_ref()
            .expect("loaded directories")
            .get(path))
    }

    fn loaded_ids(&self) -> EngineResult<MutexGuard<'_, Option<DirectoryIds>>> {
        let mut ids = self
            .ids
            .lock()
            .map_err(|_| corrupt("directory identity lock poisoned"))?;
        if ids.is_none() {
            let mut loaded = DirectoryIds::default();
            let iterator = native(
                self.collection.iter_with_options(None, false),
                "iterate directories",
            )?;
            let mut parents = Vec::new();
            for doc in iterator {
                let doc = native(doc, "read directory")?;
                let id = DirectoryId::new(u32_field(&doc, "directory_id")?);
                if doc_key(&doc)? != format!("d{}", id.get()) {
                    return Err(corrupt("directory identity differs from primary key"));
                }
                let relative_path = decode_path(&string_field(&doc, "path")?)?;
                parents.push((
                    relative_path.clone(),
                    native(doc.get_u32("parent_directory_id"), "read parent directory")?,
                ));
                loaded.claim(DirectoryRecord { id, relative_path })?;
            }
            for (path, parent) in parents {
                let expected = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(SourcePath::new)
                    .transpose()?
                    .map(|parent| loaded.get(&parent).map(DirectoryId::get));
                if expected.is_some_and(|id| id.is_none()) || expected.flatten() != parent {
                    return Err(corrupt(
                        "directory references a missing or inconsistent parent",
                    ));
                }
            }
            *ids = Some(loaded);
        }
        Ok(ids)
    }
    pub(super) fn ensure(&self, path: &SourcePath) -> EngineResult<Vec<DirectoryId>> {
        self.assert_writable()?;
        let mut guard = self.loaded_ids()?;
        let ids = guard.as_mut().expect("loaded directories");
        let result = (|| {
            let missing = path
                .ancestors()
                .skip(1)
                .filter(|path| !path.as_os_str().is_empty())
                .map(SourcePath::new)
                .collect::<EngineResult<Vec<_>>>()?
                .into_iter()
                .filter(|path| ids.get(path).is_none())
                .collect::<Vec<_>>();
            let directory_ids = ids.resolve(path)?;
            // Each child is written after its parent. Empty directories are harmless,
            // so they can be retained if the subsequent file replacement fails.
            for path in missing.iter().rev() {
                write_docs(
                    &self.collection,
                    &[encode_directory_doc(path, ids)?],
                    "write directory",
                )?;
            }
            Ok(directory_ids)
        })();
        if result.is_err() {
            // A retry must reload the subset that actually reached the collection.
            *guard = None;
        }
        result
    }

    pub(super) fn flush(&self) -> EngineResult<()> {
        self.assert_writable()?;
        native(self.collection.flush(), "flush directories")
    }

    fn assert_writable(&self) -> EngineResult<()> {
        if self.read_only {
            Err(EngineError::invalid_argument(
                "cannot modify read-only index storage",
            ))
        } else {
            Ok(())
        }
    }
}

fn directories_schema() -> EngineResult<CollectionSchema> {
    let mut schema = native(
        CollectionSchema::new("directories"),
        "create directories schema",
    )?;
    scalar(&mut schema, "directory_id", DataType::Uint32, false, true)?;
    scalar(
        &mut schema,
        "parent_directory_id",
        DataType::Uint32,
        true,
        true,
    )?;
    scalar(&mut schema, "path", DataType::String, false, false)?;
    Ok(schema)
}

fn encode_directory_doc(path: &SourcePath, ids: &DirectoryIds) -> EngineResult<Doc> {
    let id = ids
        .get(path)
        .ok_or_else(|| corrupt("unallocated directory"))?;
    let mut doc = native(Doc::new(), "create directory record")?;
    doc.set_pk(&format!("d{}", id.get()));
    native(doc.add_u32("directory_id", id.get()), "encode directory ID")?;
    native(
        doc.add_string("path", &encode_path(path)?),
        "encode directory path",
    )?;
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        let id = ids
            .get(&SourcePath::new(parent)?)
            .ok_or_else(|| corrupt("unallocated parent directory"))?;
        native(
            doc.add_u32("parent_directory_id", id.get()),
            "encode parent directory",
        )?;
    }
    Ok(doc)
}

#[cfg(test)]
mod tests;
