//! Source file identities, snapshots, and indexing status.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
};
use zvec_rust::{Collection, CollectionSchema, DataType, Doc, SearchQuery};

use super::{
    path::{
        decode_path, encode_path, file_membership_doc, file_membership_schema, path_key, query_path,
    },
    types::StoredFileAttributes,
    zvec::{
        corrupt, doc_key, fetch_map, native, open_collection, quote, scalar, string_field,
        u32_field, wildcard_string, write_docs,
    },
};
use crate::{
    EngineError, EngineResult,
    domain::{DirectoryId, FileId, FileIndexStatus, FileRecord, IndexLayout, SourcePath},
};

mod codec;
mod ids;
use ids::FileIds;

const PATH_LOOKUP_BATCH: usize = 256;

pub(super) struct Files {
    collection: Collection,
    ids: Option<FileIds>,
}

impl Files {
    #[cfg(test)]
    pub(super) fn collection(&self) -> &Collection {
        &self.collection
    }

    pub(super) fn open(layout: IndexLayout<'_>, read_only: bool) -> EngineResult<Self> {
        let ids = if read_only {
            None
        } else {
            Some(FileIds::open(layout)?)
        };
        let mut files = Self {
            collection: open_collection(&layout.files_dir(), &files_schema()?, read_only)?,
            ids,
        };
        if !read_only {
            let paths = files.list_paths()?;
            files
                .ids
                .as_mut()
                .expect("writable file identities")
                .restore(paths)?;
        }
        Ok(files)
    }

    pub(super) fn list(&self) -> EngineResult<Vec<FileRecord>> {
        let iterator = native(
            self.collection.iter_with_options(None, false),
            "iterate source files",
        )?;
        let mut files = iterator
            .map(|doc| decode_file_doc(&native(doc, "read source file")?))
            .collect::<EngineResult<Vec<_>>>()?;
        files.sort_by(|left, right| {
            left.relative_path()
                .cmp(right.relative_path())
                .then_with(|| left.id().cmp(&right.id()))
        });
        Ok(files)
    }

    /// Enumerate complete native paths without loading file snapshots or status.
    pub(super) fn list_paths(&self) -> EngineResult<Vec<(FileId, PathBuf)>> {
        let iterator = native(
            self.collection
                .iter_with_options(Some(&["file_id", "path"]), false),
            "iterate source paths",
        )?;
        iterator
            .map(|doc| decode_file_path_doc(&native(doc, "read source path")?))
            .collect()
    }

    /// Look up existing native paths through the exact-path index, without allocating IDs.
    pub(super) fn lookup_paths(
        &self,
        paths: &HashSet<PathBuf>,
    ) -> EngineResult<Vec<(FileId, PathBuf)>> {
        let requested = paths
            .iter()
            .map(|path| {
                let path = SourcePath::new(path)?;
                Ok((path_key(&path)?, path))
            })
            .collect::<EngineResult<BTreeMap<_, _>>>()?;
        let keys = requested.keys().collect::<Vec<_>>();
        let mut found = BTreeMap::new();
        let mut found_keys = HashSet::new();
        for batch in keys.chunks(PATH_LOOKUP_BATCH) {
            // One row per path is valid; the extra row exposes duplicate identities
            // instead of silently truncating an overfull result.
            let mut query = native(
                SearchQuery::scalar(i32::try_from(batch.len() + 1).expect("path batch fits i32")),
                "create exact path query",
            )?;
            let values = batch.iter().map(|key| quote(key)).collect::<Vec<_>>();
            native(
                query.set_filter(&format!("path_key IN ({})", values.join(","))),
                "filter exact source paths",
            )?;
            native(
                query.set_output_fields(&["file_id", "path", "path_key"]),
                "project source path identities",
            )?;
            native(query.set_include_vector(false), "omit source vectors")?;
            for doc in native(self.collection.query(&query), "query exact source paths")? {
                let key = string_field(&doc, "path_key")?;
                let (id, path) = decode_file_path_doc(&doc)?;
                if batch
                    .binary_search_by(|expected| expected.as_str().cmp(key.as_str()))
                    .is_err()
                    || requested.get(&key).map(SourcePath::as_path) != Some(path.as_path())
                {
                    return Err(corrupt("source path query returned a conflicting identity"));
                }
                if found.insert(id, path).is_some() || !found_keys.insert(key) {
                    return Err(corrupt("source path query returned duplicate identities"));
                }
            }
        }
        Ok(found.into_iter().collect())
    }

    pub(super) fn list_attributes(&self) -> EngineResult<Vec<StoredFileAttributes>> {
        let iterator = native(
            self.collection
                .iter_with_options(Some(&["file_id", "path", "modified_epoch_ms"]), false),
            "iterate source attributes",
        )?;
        iterator
            .map(|doc| decode_file_attributes_doc(&native(doc, "read source attributes")?))
            .collect()
    }

    pub(super) fn resolve_ids(&mut self, paths: &[PathBuf]) -> EngineResult<Vec<FileId>> {
        self.ids
            .as_mut()
            .ok_or_else(read_only_error)?
            .resolve(paths)
    }

    pub(super) fn validate(&self, file: &FileRecord) -> EngineResult<()> {
        self.ids
            .as_ref()
            .ok_or_else(read_only_error)?
            .validate(file)
    }

    /// Readers query the native index without loading the entire identity map.
    pub(super) fn has_non_unicode_file_names(&self) -> EngineResult<bool> {
        if let Some(ids) = &self.ids {
            return Ok(ids.has_non_unicode_file_names());
        }
        let mut query = native(SearchQuery::scalar(1), "check native file names")?;
        native(
            query.set_filter("file_name = ''"),
            "filter native file names",
        )?;
        native(
            query.set_output_fields(&["file_id"]),
            "project file identity",
        )?;
        native(query.set_include_vector(false), "omit vectors")?;
        Ok(!native(self.collection.query(&query), "query native file names")?.is_empty())
    }

    pub(super) fn fetch(&self, ids: &[FileId]) -> EngineResult<HashMap<FileId, FileRecord>> {
        let keys = ids.iter().copied().map(file_key).collect::<Vec<_>>();
        fetch_map(&self.collection, &keys)?
            .into_values()
            .map(|doc| {
                let file = decode_file_doc(&doc)?;
                Ok((file.id(), file))
            })
            .collect()
    }

    pub(super) fn put(&self, file: &FileRecord, directories: &[DirectoryId]) -> EngineResult<()> {
        self.validate(file)?;
        write_docs(
            &self.collection,
            &[encode_file_doc(file, directories)?],
            "write source file",
        )
    }

    /// Preserve every query projection while recording a resumable deletion.
    pub(super) fn mark_deleting(&self, id: FileId) -> EngineResult<()> {
        self.assert_writable()?;
        let key = file_key(id);
        if let Some(mut doc) = fetch_map(&self.collection, std::slice::from_ref(&key))?.remove(&key)
        {
            let mut file = decode_file_doc(&doc)?;
            file.set_index_status(FileIndexStatus::Deleting)?;
            native(
                doc.add_string("payload", &codec::encode_file(&file)?),
                "encode deleting source file",
            )?;
            write_docs(&self.collection, &[doc], "mark source file for deletion")?;
        }
        Ok(())
    }

    pub(super) fn delete(&mut self, id: FileId) -> EngineResult<()> {
        self.assert_writable()?;
        native(
            self.collection
                .delete_by_filter(&format!("file_id = {}", id.get())),
            "delete source file",
        )?;
        self.ids
            .as_mut()
            .expect("writable file identities")
            .remove(id);
        Ok(())
    }

    pub(super) fn flush(&self) -> EngineResult<()> {
        self.assert_writable()?;
        native(self.collection.flush(), "flush source files")
    }

    pub(super) fn optimize(&self) -> EngineResult<()> {
        self.assert_writable()?;
        native(self.collection.optimize(), "optimize source files")
    }

    fn assert_writable(&self) -> EngineResult<()> {
        self.ids.as_ref().map(|_| ()).ok_or_else(read_only_error)
    }
}

fn read_only_error() -> EngineError {
    EngineError::invalid_argument("cannot modify read-only index storage")
}

fn file_key(id: FileId) -> String {
    format!("f{}", id.get())
}

fn files_schema() -> EngineResult<CollectionSchema> {
    let mut schema = native(CollectionSchema::new("files"), "create files schema")?;
    scalar(&mut schema, "file_id", DataType::Uint32, false, true)?;
    scalar(&mut schema, "path_key", DataType::String, false, true)?;
    // Native paths are stored separately from their optional Unicode projection.
    scalar(&mut schema, "path", DataType::String, false, false)?;
    wildcard_string(&mut schema, "relative_path", true)?;
    file_membership_schema(&mut schema)?;
    scalar(
        &mut schema,
        "modified_epoch_ms",
        DataType::Uint64,
        true,
        false,
    )?;
    scalar(&mut schema, "payload", DataType::String, false, false)?;
    Ok(schema)
}

fn encode_file_doc(file: &FileRecord, directories: &[DirectoryId]) -> EngineResult<Doc> {
    let mut doc = native(Doc::new(), "create file record")?;
    let key = file_key(file.id());
    doc.set_pk(&key);
    native(
        doc.add_u32("file_id", file.id().get()),
        "encode file identity",
    )?;
    native(
        doc.add_string("path_key", &path_key(file.relative_path())?),
        "encode exact path key",
    )?;
    native(
        doc.add_string("path", &encode_path(file.relative_path())?),
        "encode native file path",
    )?;
    if let Some(path) = query_path(file.relative_path()) {
        native(
            doc.add_string("relative_path", &path),
            "encode queryable file path",
        )?;
    }
    file_membership_doc(&mut doc, file, directories)?;
    if let Some(modified) = file.snapshot().modified_epoch_ms {
        native(
            doc.add_u64("modified_epoch_ms", modified),
            "encode file modification time",
        )?;
    }
    native(
        doc.add_string("payload", &codec::encode_file(file)?),
        "encode file payload",
    )?;
    Ok(doc)
}

fn decode_file_doc(doc: &Doc) -> EngineResult<FileRecord> {
    let file = codec::decode_file(&string_field(doc, "payload")?)?;
    let attributes = decode_file_attributes_doc(doc)?;
    if attributes.id != file.id()
        || attributes.relative_path != file.relative_path().as_path()
        || attributes.modified_epoch_ms != file.snapshot().modified_epoch_ms
        || string_field(doc, "path_key")? != path_key(file.relative_path())?
    {
        return Err(corrupt("file attributes differ from their stored payload"));
    }
    Ok(file)
}

fn decode_file_attributes_doc(doc: &Doc) -> EngineResult<StoredFileAttributes> {
    let (id, relative_path) = decode_file_path_doc(doc)?;
    Ok(StoredFileAttributes {
        id,
        relative_path,
        modified_epoch_ms: native(
            doc.get_u64("modified_epoch_ms"),
            "read file modification time",
        )?,
    })
}

fn decode_file_path_doc(doc: &Doc) -> EngineResult<(FileId, PathBuf)> {
    let id = FileId::new(u32_field(doc, "file_id")?);
    if doc_key(doc)? != file_key(id) {
        return Err(corrupt("file identity differs from its primary key"));
    }
    Ok((
        id,
        decode_path(&string_field(doc, "path")?)?.into_path_buf(),
    ))
}

#[cfg(test)]
mod tests;
