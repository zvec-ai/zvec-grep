use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use zvec_rust::{
    Collection, CollectionOptions, CollectionSchema, DataType, Doc, FieldSchema, Fts, IndexParams,
    MetricType, SearchQuery,
};

use super::{
    codec,
    directories::DirectoryIds,
    file_ids::FileIds,
    path::{decode_path, encode_path, path_key, query_path},
    spi::{
        IndexedFragment, StoragePathFilter, StorageSearchFilter, StorageSearchHit,
        StorageSearchPath, StoredEntity, StoredFileAttributes, StoredSearchData,
    },
};
use crate::domain::{FTS_CONFIG, model::EmbeddingModelInfo};
use crate::{
    EngineError, EngineResult,
    domain::{
        CodeMetadata, Content, DirectoryId, DirectoryRecord, Entity, EntityFragment, EntityId,
        EntityMetadata, FileId, FileRecord, IndexField, SourcePath, model::Metric,
        validate_entities,
    },
    utils::sha256_hex_parts,
};

const WRITE_BATCH: usize = 1024;
const MAX_TOP_K: usize = 100_000;

pub(super) struct NativeStore {
    files: Collection,
    directories: Collection,
    directory_ids: Mutex<Option<DirectoryIds>>,
    entities: Collection,
    indexes: BTreeMap<String, ModelIndex>,
    read_only: bool,
}

struct ModelIndex {
    collection: Collection,
    dimension: usize,
}

struct EncodedMetadata {
    json: String,
    fields: Vec<(IndexField, String)>,
}

impl EncodedMetadata {
    fn new(metadata: &EntityMetadata) -> EngineResult<Self> {
        let value = serde_json::to_value(metadata)
            .map_err(|error| corrupt(format!("encode entity metadata: {error}")))?;
        let fields = encode_metadata_fields(&value)?;
        let json = serde_json::to_string(&value)
            .map_err(|error| corrupt(format!("encode entity metadata: {error}")))?;
        Ok(Self { json, fields })
    }
}

impl NativeStore {
    pub(super) fn open(
        path: &Path,
        embeddings: &[EmbeddingModelInfo],
        read_only: bool,
    ) -> EngineResult<Self> {
        let files = open_collection(&path.join("files"), &files_schema()?, read_only)?;
        let directories =
            open_collection(&path.join("directories"), &directories_schema()?, read_only)?;
        let entities = open_collection(&path.join("entities"), &entities_schema()?, read_only)?;
        let mut indexes = BTreeMap::new();
        for embedding in embeddings {
            let dimension = u32::try_from(embedding.dimension)
                .ok()
                .filter(|value| (1..=20_000).contains(value))
                .ok_or_else(|| {
                    EngineError::invalid_argument(
                        "storage embedding dimension must be between 1 and 20,000",
                    )
                })?;
            let metric = match embedding.metric {
                Metric::Cosine => MetricType::Cosine,
                Metric::DotProduct => MetricType::Ip,
                Metric::Euclidean => MetricType::L2,
            };
            let name = fragment_collection_name(embedding);
            let collection = open_collection(
                &path.join(&name),
                &fragments_schema(dimension, metric)?,
                read_only,
            )?;
            if indexes
                .insert(
                    embedding.model.reference(),
                    ModelIndex {
                        collection,
                        dimension: embedding.dimension,
                    },
                )
                .is_some()
            {
                return Err(EngineError::invalid_argument("duplicate embedding model"));
            }
        }
        Ok(Self {
            directory_ids: Mutex::new(None),
            files,
            directories,
            entities,
            indexes,
            read_only,
        })
    }

    pub(super) fn list_files(&self) -> EngineResult<Vec<FileRecord>> {
        let iterator = native(
            self.files.iter_with_options(None, false),
            "iterate source files",
        )?;
        let mut files = iterator
            .map(|doc| decode_file_doc(&native(doc, "read source file")?))
            .collect::<EngineResult<Vec<_>>>()?;
        files.sort_by(|left, right| {
            left.relative_path
                .cmp(&right.relative_path)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(files)
    }

    /// Enumerate complete native paths without loading file snapshots or status.
    pub(super) fn list_file_paths(&self) -> EngineResult<Vec<(FileId, PathBuf)>> {
        let iterator = native(
            self.files
                .iter_with_options(Some(&["file_id", "path"]), false),
            "iterate source paths",
        )?;
        iterator
            .map(|doc| decode_file_path_doc(&native(doc, "read source path")?))
            .collect()
    }

    pub(super) fn list_file_attributes(&self) -> EngineResult<Vec<StoredFileAttributes>> {
        let iterator = native(
            self.files
                .iter_with_options(Some(&["file_id", "path", "modified_epoch_ms"]), false),
            "iterate source attributes",
        )?;
        iterator
            .map(|doc| decode_file_attributes_doc(&native(doc, "read source attributes")?))
            .collect()
    }

    /// Source identities and directory identities each have their own authority.
    pub(super) fn load_file_ids(&self) -> EngineResult<FileIds> {
        let ids = FileIds::from_paths(self.list_file_paths()?)?;
        let _directories = self.directory_ids()?;
        Ok(ids)
    }

    fn directory_ids(&self) -> EngineResult<MutexGuard<'_, Option<DirectoryIds>>> {
        let mut ids = self
            .directory_ids
            .lock()
            .map_err(|_| corrupt("directory identity lock poisoned"))?;
        if ids.is_none() {
            let mut loaded = DirectoryIds::default();
            let iterator = native(
                self.directories.iter_with_options(None, false),
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
    /// Non-Unicode basenames use an empty, non-null indexed projection.
    pub(super) fn has_non_unicode_file_names(&self) -> EngineResult<bool> {
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
        Ok(!native(self.files.query(&query), "query native file names")?.is_empty())
    }

    pub(super) fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        if limit == 0 || empty_filter(filter) {
            return Ok(Vec::new());
        }
        let mut fts = native(Fts::new(), "create full-text request")?;
        native(
            fts.set_match_string(&index_text(query)),
            "set full-text query",
        )?;
        let mut request = native(
            SearchQuery::fts("text", &fts, top_k(limit)?),
            "create full-text query",
        )?;
        configure_query(
            self,
            &mut request,
            filter,
            &["document_id", "entity_id", "file_id"],
        )?;
        let mut hits = Vec::new();
        for index in self.indexes.values() {
            for (rank, doc) in native(index.collection.query(&request), "search full-text index")?
                .into_iter()
                .enumerate()
            {
                let mut hit = decode_search_hit(&doc, StorageSearchPath::Fts)?;
                // BM25 is computed against each table's corpus; merge independent rankings.
                if self.indexes.len() > 1 {
                    let rank = u32::try_from(rank + 1)
                        .map_err(|_| corrupt("FTS rank exceeds query limit"))?;
                    hit.score = 1.0 / (60.0 + f64::from(rank));
                }
                hits.push(hit);
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.document_id.cmp(&b.document_id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    pub(super) fn search_vector(
        &self,
        model: &str,
        vector: &[f32],
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        let index = self.index(model)?;
        self.validate_vector(model, vector)?;
        if limit == 0 || empty_filter(filter) {
            return Ok(Vec::new());
        }
        let mut request = native(
            SearchQuery::new("embedding", vector, top_k(limit)?),
            "create vector query",
        )?;
        configure_query(
            self,
            &mut request,
            filter,
            &["document_id", "entity_id", "file_id"],
        )?;
        native(index.collection.query(&request), "search vector index")?
            .into_iter()
            .map(|doc| decode_search_hit(&doc, StorageSearchPath::Vector))
            .collect()
    }

    pub(super) fn load_search_hits(
        &self,
        hits: &[StorageSearchHit],
    ) -> EngineResult<StoredSearchData> {
        let keys = |values: Vec<String>| {
            values
                .into_iter()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        };
        let entity_keys = keys(hits.iter().map(|hit| entity_key(&hit.entity_id)).collect());
        let file_keys = keys(hits.iter().map(|hit| file_key(hit.file_id)).collect());
        let entity_docs = fetch_map(&self.entities, &entity_keys)?;
        let files = fetch_map(&self.files, &file_keys)?
            .into_values()
            .map(|doc| decode_file_doc(&doc).map(|file| (file.id, file)))
            .collect::<EngineResult<HashMap<_, _>>>()?;
        let mut entities = HashMap::new();
        let mut fragments = HashMap::new();
        for doc in entity_docs.into_values() {
            let metadata = decode_metadata(&doc)?;
            let entity = codec::decode_entity(&string_field(&doc, "payload")?, metadata.as_ref())?;
            if doc_key(&doc)? != entity_key(&entity.id)
                || u32_field(&doc, "file_id")? != entity.file_id.get()
                || string_field(&doc, "entity_id")? != hex::encode(entity.id.as_str())
            {
                return Err(corrupt("entity identity differs from its index fields"));
            }
            let file = files
                .get(&entity.file_id)
                .ok_or_else(|| corrupt("entity references a missing file"))?;
            validate_indexed_owner(file, entity.file_id)?;
            for fragment in &entity.fragments {
                if fragments
                    .insert(fragment.id.as_str().to_owned(), fragment.clone())
                    .is_some()
                {
                    return Err(corrupt("duplicate canonical fragment identity"));
                }
            }
            entities.insert(
                entity.id.clone(),
                StoredEntity {
                    entity,
                    file: file.clone(),
                },
            );
        }
        for hit in hits {
            let owner = entities
                .get(&hit.entity_id)
                .ok_or_else(|| corrupt("search references a missing entity"))?;
            if owner.file.id != hit.file_id
                || !owner
                    .entity
                    .fragments
                    .iter()
                    .any(|fragment| fragment.id.as_str() == hit.document_id)
            {
                return Err(corrupt(
                    "search identities differ from their stored fragment",
                ));
            }
        }
        Ok(StoredSearchData {
            entities,
            fragments,
        })
    }

    /// Complete validation precedes the first native mutation of a file.
    fn validate_replacement(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
    ) -> EngineResult<()> {
        validate_entities(file.id, entities)?;
        validate_projections(entities, entries)?;
        let count = u64::try_from(entities.len())
            .map_err(|_| EngineError::invalid_argument("entity count exceeds u64"))?;
        if (!file.index_status.is_indexed() && (!entities.is_empty() || !entries.is_empty()))
            || file.index_status.entity_count() != count
        {
            return Err(EngineError::invalid_argument(
                "file index status does not match its entities",
            ));
        }
        for entry in entries {
            self.validate_vector(&entry.model, &entry.vector)?;
        }
        let fragment_keys = entries
            .iter()
            .map(|entry| primary_key("fragment", entry.fragment_id.as_str()))
            .collect::<Vec<_>>();
        for collection in self.indexes.values().map(|index| &index.collection) {
            reject_foreign_ids(collection, &fragment_keys, file.id)?;
        }
        let entity_keys = entities
            .iter()
            .map(|entity| entity_key(&entity.id))
            .collect::<Vec<_>>();
        reject_foreign_ids(&self.entities, &entity_keys, file.id)
    }

    pub(super) fn apply_replace(
        &self,
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
    ) -> EngineResult<()> {
        self.assert_writable()?;
        self.validate_replacement(file, entities, entries)?;
        let (directory_ids, directory_docs) = {
            let mut guard = self.directory_ids()?;
            let ids = guard.as_mut().expect("loaded directories");
            // Directories are immutable identities. Only create newly allocated rows;
            // rewriting shared ancestors for every file would amplify writes and put
            // otherwise checkpointed directory identities back into the native WAL.
            let missing = file
                .relative_path
                .ancestors()
                .skip(1)
                .filter(|path| !path.as_os_str().is_empty())
                .map(SourcePath::new)
                .collect::<EngineResult<Vec<_>>>()?
                .into_iter()
                .filter(|path| ids.get(path).is_none())
                .collect::<Vec<_>>();
            let directory_ids = ids.resolve(&file.relative_path)?;
            let directory_docs = missing
                .iter()
                .map(|path| encode_directory_doc(path, ids))
                .collect::<EngineResult<Vec<_>>>()?;
            (directory_ids, directory_docs)
        };
        let file_doc = encode_file_doc(file, &directory_ids)?;
        let owners = entities
            .iter()
            .map(|entity| {
                Ok((
                    &entity.id,
                    (
                        entity,
                        entity
                            .metadata
                            .as_ref()
                            .map(EncodedMetadata::new)
                            .transpose()?,
                    ),
                ))
            })
            .collect::<EngineResult<HashMap<_, _>>>()?;
        let fragments = entities
            .iter()
            .flat_map(|entity| &entity.fragments)
            .map(|fragment| (&fragment.id, fragment))
            .collect::<HashMap<_, _>>();
        let mut projections: BTreeMap<&str, Vec<Doc>> = BTreeMap::new();
        for entry in entries {
            let (owner, metadata) = &owners[&entry.entity_id];
            let fragment = fragments[&entry.fragment_id];
            let mut doc = fragment_doc(owner, fragment, file, &directory_ids, metadata.as_ref())?;
            native(
                doc.add_string("text", &lexical_text(owner, fragment)?),
                "encode searchable text",
            )?;
            native(
                doc.add_vector_f32("embedding", &entry.vector),
                "encode embedding vector",
            )?;
            projections.entry(&entry.model).or_default().push(doc);
        }
        let entity_docs = entities
            .iter()
            .map(|entity| {
                let mut doc = native(Doc::new(), "create canonical entity record")?;
                doc.set_pk(&entity_key(&entity.id));
                native(
                    doc.add_u32("file_id", entity.file_id.get()),
                    "encode entity file",
                )?;
                native(
                    doc.add_string("entity_id", &hex::encode(entity.id.as_str())),
                    "encode entity identity",
                )?;
                native(
                    doc.add_string("payload", &codec::encode_entity(entity)?),
                    "encode canonical entity",
                )?;
                if let Some(metadata) = &owners[&entity.id].1 {
                    native(
                        doc.add_string("metadata", &metadata.json),
                        "encode entity metadata",
                    )?;
                }
                Ok(doc)
            })
            .collect::<EngineResult<Vec<_>>>()?;
        self.delete_documents(file.id)?;
        write_docs(&self.directories, &directory_docs, "write directories")?;
        write_docs(
            &self.entities,
            &entity_docs,
            "write entities and canonical fragments",
        )?;
        for (model, docs) in projections {
            write_docs(
                &self.index(model)?.collection,
                &docs,
                "write model fragments",
            )?;
        }
        write_docs(&self.files, &[file_doc], "publish source file")
    }

    pub(super) fn apply_delete(&self, id: FileId) -> EngineResult<()> {
        self.assert_writable()?;
        self.delete_documents(id)?;
        native(
            self.files
                .delete_by_filter(&format!("file_id = {}", id.get())),
            "delete source file",
        )
    }

    fn delete_documents(&self, id: FileId) -> EngineResult<()> {
        let filter = format!("file_id = {}", id.get());
        for collection in self
            .indexes
            .values()
            .map(|index| &index.collection)
            .chain(std::iter::once(&self.entities))
        {
            native(
                collection.delete_by_filter(&filter),
                "delete source fragments and entities",
            )?;
        }
        Ok(())
    }

    pub(super) fn flush(&self) -> EngineResult<()> {
        self.assert_writable()?;
        for collection in std::iter::once(&self.directories)
            .chain(std::iter::once(&self.entities))
            .chain(self.indexes.values().map(|index| &index.collection))
            .chain(std::iter::once(&self.files))
        {
            native(collection.flush(), "flush storage collection")?;
        }
        Ok(())
    }

    fn assert_writable(&self) -> EngineResult<()> {
        if self.read_only {
            Err(EngineError::permission_denied(
                "cannot modify read-only index storage",
            ))
        } else {
            Ok(())
        }
    }

    fn index(&self, model: &str) -> EngineResult<&ModelIndex> {
        self.indexes.get(model).ok_or_else(|| {
            EngineError::invalid_argument(format!("unknown embedding model {model:?}"))
        })
    }

    fn validate_vector(&self, model: &str, vector: &[f32]) -> EngineResult<()> {
        let dimension = self.index(model)?.dimension;
        if vector.len() != dimension || vector.iter().any(|value| !value.is_finite()) {
            return Err(EngineError::invalid_argument(format!(
                "expected {} finite embedding values, got {}",
                dimension,
                vector.len()
            )));
        }
        Ok(())
    }
}

pub(super) fn fragment_collection_name(embedding: &EmbeddingModelInfo) -> String {
    format!(
        "fragments_{}",
        primary_key(
            "space",
            &format!(
                "{}\0{}\0{}\0{:?}",
                embedding.model.provider,
                embedding.model.name,
                embedding.dimension,
                embedding.metric
            )
        )
    )
}

fn open_collection(
    path: &Path,
    schema: &CollectionSchema,
    read_only: bool,
) -> EngineResult<Collection> {
    let text = native_path(path)?;
    let mut options = native(CollectionOptions::new(), "configure collection")?;
    native(
        options.set_read_only(read_only),
        "set collection access mode",
    )?;
    if path.exists() {
        native(
            Collection::open(text, Some(&options)),
            &format!("open collection {}", path.display()),
        )
    } else if read_only {
        Err(EngineError::not_found(format!(
            "index collection does not exist: {}",
            path.display()
        )))
    } else {
        native(
            Collection::create_and_open(text, schema, Some(&options)),
            &format!("create collection {}", path.display()),
        )
    }
}

fn native_path(path: &Path) -> EngineResult<&str> {
    // zvec rejects the `?` in Windows verbatim prefixes. Simplify only when
    // the regular path identifies the same location; retain internal paths.
    #[cfg(windows)]
    let path = dunce::simplified(path);
    path.to_str().ok_or_else(|| {
        EngineError::invalid_argument(format!(
            "zvec storage path must be UTF-8: {}",
            path.display()
        ))
    })
}

fn scalar(
    schema: &mut CollectionSchema,
    name: &str,
    data_type: DataType,
    nullable: bool,
    indexed: bool,
) -> EngineResult<()> {
    let mut field = native(
        FieldSchema::new(name, data_type, nullable, 0),
        "define scalar field",
    )?;
    if indexed {
        native(
            field.set_index_params(&native(
                IndexParams::invert(true, false),
                "define scalar index",
            )?),
            "attach scalar index",
        )?;
    }
    native(schema.add_field(&field), "add scalar field")
}

fn identity_schema(name: &str) -> EngineResult<CollectionSchema> {
    let mut schema = native(CollectionSchema::new(name), "create collection schema")?;
    scalar(&mut schema, "file_id", DataType::Uint32, false, true)?;
    scalar(&mut schema, "entity_id", DataType::String, false, true)?;
    scalar(&mut schema, "document_id", DataType::String, false, false)?;
    Ok(schema)
}

fn retrieval_schema(name: &str) -> EngineResult<CollectionSchema> {
    let mut schema = identity_schema(name)?;
    file_membership_schema(&mut schema)?;
    for field in EntityMetadata::index_schema() {
        match field {
            IndexField::String(name) => {
                scalar(&mut schema, name, DataType::String, true, true)?;
            }
        }
    }
    Ok(schema)
}

fn file_membership_schema(schema: &mut CollectionSchema) -> EngineResult<()> {
    scalar(
        schema,
        "ancestor_directory_ids",
        DataType::ArrayUint32,
        false,
        true,
    )?;
    wildcard_string(schema, "file_name", false)
}

fn wildcard_string(schema: &mut CollectionSchema, name: &str, nullable: bool) -> EngineResult<()> {
    let mut field = native(
        FieldSchema::new(name, DataType::String, nullable, 0),
        "define indexed path string",
    )?;
    native(
        field.set_index_params(&native(
            IndexParams::invert(false, true),
            "define path string index",
        )?),
        "attach path string index",
    )?;
    native(schema.add_field(&field), "add indexed path string")
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

fn entities_schema() -> EngineResult<CollectionSchema> {
    let mut schema = native(CollectionSchema::new("entities"), "create entities schema")?;
    scalar(&mut schema, "file_id", DataType::Uint32, false, true)?;
    scalar(&mut schema, "entity_id", DataType::String, false, true)?;
    scalar(&mut schema, "payload", DataType::String, false, false)?;
    scalar(&mut schema, "metadata", DataType::String, true, false)?;
    Ok(schema)
}

fn fragments_schema(dimension: u32, metric: MetricType) -> EngineResult<CollectionSchema> {
    let mut schema = retrieval_schema("fragments")?;
    let mut text = native(
        FieldSchema::new("text", DataType::String, false, 0),
        "define full-text field",
    )?;
    native(
        text.set_index_params(&native(
            IndexParams::fts(Some(FTS_CONFIG.tokenizer), Some(FTS_CONFIG.filters), None),
            "define full-text index",
        )?),
        "attach full-text index",
    )?;
    native(schema.add_field(&text), "add full-text field")?;

    let mut vector = native(
        FieldSchema::new("embedding", DataType::VectorFp32, false, dimension),
        "define vector field",
    )?;
    native(
        vector.set_index_params(&native(
            IndexParams::hnsw(metric, 16, 200),
            "define vector index",
        )?),
        "attach vector index",
    )?;
    native(schema.add_field(&vector), "add vector field")?;
    Ok(schema)
}

fn validate_indexed_owner(file: &FileRecord, file_id: FileId) -> EngineResult<()> {
    if file.id != file_id
        || !file.index_status.is_indexed()
        || file.index_status.entity_count() == 0
    {
        return Err(corrupt(
            "fragment references a file without a successful index",
        ));
    }
    Ok(())
}

fn encode_file_doc(file: &FileRecord, directories: &[DirectoryId]) -> EngineResult<Doc> {
    let mut doc = native(Doc::new(), "create file record")?;
    let key = file_key(file.id);
    doc.set_pk(&key);
    native(
        doc.add_u32("file_id", file.id.get()),
        "encode file identity",
    )?;
    native(
        doc.add_string("path_key", &path_key(&file.relative_path)?),
        "encode exact path key",
    )?;
    native(
        doc.add_string("path", &encode_path(&file.relative_path)?),
        "encode native file path",
    )?;
    if let Some(path) = query_path(&file.relative_path) {
        native(
            doc.add_string("relative_path", &path),
            "encode queryable file path",
        )?;
    }
    file_membership_doc(&mut doc, file, directories)?;
    if let Some(modified) = file.snapshot.modified_epoch_ms {
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
    if attributes.id != file.id
        || attributes.relative_path != file.relative_path.as_path()
        || attributes.modified_epoch_ms != file.snapshot.modified_epoch_ms
        || string_field(doc, "path_key")? != path_key(&file.relative_path)?
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

fn identity_doc(entity: &Entity, fragment: &EntityFragment) -> EngineResult<Doc> {
    let mut doc = native(Doc::new(), "create fragment record")?;
    doc.set_pk(&primary_key("fragment", fragment.id.as_str()));
    native(
        doc.add_u32("file_id", entity.file_id.get()),
        "encode source identity",
    )?;
    native(
        doc.add_string("entity_id", &hex::encode(entity.id.as_str())),
        "encode entity identity",
    )?;
    native(
        doc.add_string("document_id", &hex::encode(fragment.id.as_str())),
        "encode document identity",
    )?;
    Ok(doc)
}

fn fragment_doc(
    entity: &Entity,
    fragment: &EntityFragment,
    file: &FileRecord,
    directories: &[DirectoryId],
    metadata: Option<&EncodedMetadata>,
) -> EngineResult<Doc> {
    let mut doc = identity_doc(entity, fragment)?;
    file_membership_doc(&mut doc, file, directories)?;
    if let Some(metadata) = metadata {
        for (field, value) in &metadata.fields {
            native(
                doc.add_string(field.name(), value),
                "encode metadata index field",
            )?;
        }
    }
    Ok(doc)
}

fn file_membership_doc(
    doc: &mut Doc,
    file: &FileRecord,
    directories: &[DirectoryId],
) -> EngineResult<()> {
    native(
        doc.add_array_u32(
            "ancestor_directory_ids",
            &directories.iter().map(|id| id.get()).collect::<Vec<_>>(),
        ),
        "encode ancestor directories",
    )?;
    // Non-Unicode names have no STRING representation. The source-path cache disables name
    // pushdown in this workspace; exact native paths remain in FileRecord.
    let name = file
        .relative_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    native(doc.add_string("file_name", name), "encode file name")?;
    Ok(())
}

fn decode_id(encoded: &str) -> EngineResult<String> {
    let bytes = hex::decode(encoded)
        .map_err(|error| corrupt(format!("invalid encoded identity: {error}")))?;
    String::from_utf8(bytes).map_err(|error| corrupt(format!("invalid identity text: {error}")))
}

fn decode_search_hit(doc: &Doc, path: StorageSearchPath) -> EngineResult<StorageSearchHit> {
    let document_id = decode_id(&string_field(doc, "document_id")?)?;
    let entity_id = EntityId::new(decode_id(&string_field(doc, "entity_id")?)?)
        .map_err(|error| corrupt(error.to_string()))?;
    if document_id.trim().is_empty() || doc_key(doc)? != primary_key("fragment", &document_id) {
        return Err(corrupt(
            "search document identity differs from its primary key",
        ));
    }
    Ok(StorageSearchHit {
        document_id,
        entity_id,
        file_id: FileId::new(u32_field(doc, "file_id")?),
        path,
        score: f64::from(doc.get_score()),
    })
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

fn fetch_map(collection: &Collection, keys: &[String]) -> EngineResult<HashMap<String, Doc>> {
    let mut result = HashMap::with_capacity(keys.len());
    for batch in keys.chunks(WRITE_BATCH) {
        let refs = batch.iter().map(String::as_str).collect::<Vec<_>>();
        for doc in native(
            collection.fetch_with_options(&refs, None, false),
            "fetch stored records",
        )? {
            result.insert(doc_key(&doc)?.to_owned(), doc);
        }
    }
    Ok(result)
}

fn write_docs(collection: &Collection, docs: &[Doc], operation: &str) -> EngineResult<()> {
    for batch in docs.chunks(WRITE_BATCH) {
        let refs = batch.iter().collect::<Vec<_>>();
        let result = native(collection.upsert(&refs), operation)?;
        if result.results.len() != batch.len()
            || result.error_count != 0
            || result.success_count != u64::try_from(batch.len()).unwrap_or(u64::MAX)
            || result.results.iter().any(|status| !status.success)
        {
            let detail = result
                .results
                .iter()
                .find(|status| !status.success)
                .map_or_else(
                    || "incomplete write result".to_owned(),
                    |status| format!("{}: {}", status.code, status.message),
                );
            return Err(EngineError::storage_failure(format!(
                "{operation}: {detail}"
            )));
        }
    }
    Ok(())
}

fn configure_query(
    store: &NativeStore,
    query: &mut SearchQuery,
    filter: Option<&StorageSearchFilter>,
    fields: &[&str],
) -> EngineResult<()> {
    native(
        query.set_include_vector(false),
        "omit vectors from search output",
    )?;
    native(
        query.set_output_fields(fields),
        "select search output fields",
    )?;
    if let Some(filter) = build_filter(store, filter)? {
        native(query.set_filter(&filter), "set search filter")?;
    }
    Ok(())
}

fn build_filter(
    store: &NativeStore,
    filter: Option<&StorageSearchFilter>,
) -> EngineResult<Option<String>> {
    let Some(filter) = filter else {
        return Ok(None);
    };
    let mut clauses = Vec::new();
    if let Some(path) = &filter.path
        && !matches!(path, StoragePathFilter::All)
    {
        clauses.push(path_filter(store, path, false)?);
    }
    if let Some(ids) = &filter.file_ids {
        clauses.push(file_id_filter(ids));
    }
    if let Some(ids) = &filter.entity_ids {
        clauses.push(in_filter(
            "entity_id",
            ids.iter().map(|id| hex::encode(id.as_str())),
        ));
    }
    if let Some(names) = &filter.symbol_names {
        clauses.push(metadata_in_filter(
            CodeMetadata::SYMBOL_NAME,
            names.iter().map(String::as_str),
        ));
    }
    if let Some(types) = &filter.symbol_types {
        clauses.push(metadata_in_filter(
            CodeMetadata::SYMBOL_TYPE,
            types.iter().map(|kind| kind.as_str()),
        ));
    }
    Ok((!clauses.is_empty()).then(|| clauses.join(" AND ")))
}

fn encode_metadata_fields(metadata: &serde_json::Value) -> EngineResult<Vec<(IndexField, String)>> {
    let mut fields = Vec::new();
    for field in EntityMetadata::index_schema() {
        let Some(value) = metadata.get(field.name()).filter(|value| !value.is_null()) else {
            continue;
        };
        match field {
            IndexField::String(name) => {
                let value = value.as_str().ok_or_else(|| {
                    corrupt(format!("metadata index field {name:?} must be a string"))
                })?;
                fields.push((field, encode_metadata_value(field, value).into_owned()));
            }
        }
    }
    Ok(fields)
}

fn encode_metadata_value(field: IndexField, value: &str) -> Cow<'_, str> {
    if field == CodeMetadata::SYMBOL_NAME {
        Cow::Owned(primary_key("symbol", value))
    } else {
        Cow::Borrowed(value)
    }
}

fn metadata_in_filter<'a>(field: IndexField, values: impl Iterator<Item = &'a str>) -> String {
    in_filter(
        field.name(),
        values.map(|value| encode_metadata_value(field, value)),
    )
}

fn in_filter(field: &str, values: impl Iterator<Item = impl AsRef<str>>) -> String {
    format!(
        "{field} IN ({})",
        values
            .map(|value| quote(value.as_ref()))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Push negation to field operators; execution support and errors belong to zvec.
fn path_filter(
    store: &NativeStore,
    filter: &StoragePathFilter,
    negated: bool,
) -> EngineResult<String> {
    Ok(match filter {
        StoragePathFilter::All => constant_filter(!negated),
        StoragePathFilter::None => constant_filter(negated),
        StoragePathFilter::Directory(path) => {
            let Some(id) = store
                .directory_ids()?
                .as_ref()
                .expect("loaded directories")
                .get(path)
            else {
                return Ok(constant_filter(negated));
            };
            format!(
                "ancestor_directory_ids {}CONTAIN_ANY ({id})",
                if negated { "NOT " } else { "" }
            )
        }
        StoragePathFilter::FileNameExact(name) => {
            if name.contains(['\\', '\0']) {
                return Err(EngineError::invalid_argument(
                    "this file name requires path matching",
                ));
            }
            format!(
                "file_name {} {}",
                if negated { "!=" } else { "=" },
                quote(name)
            )
        }
        StoragePathFilter::FileNamePrefix(prefix) => format!(
            "file_name {}LIKE {}",
            if negated { "NOT " } else { "" },
            quote(&format!("{}%", like_literal(prefix)))
        ),
        StoragePathFilter::FileNameSuffix(suffix) => format!(
            "file_name {}LIKE {}",
            if negated { "NOT " } else { "" },
            quote(&format!("%{}", like_literal(suffix)))
        ),
        StoragePathFilter::And(filters) => boolean_filter(store, filters, !negated, negated)?,
        StoragePathFilter::Or(filters) => boolean_filter(store, filters, negated, negated)?,
        StoragePathFilter::Not(filter) => path_filter(store, filter, !negated)?,
    })
}

fn file_id_filter(ids: &[FileId]) -> String {
    if ids.is_empty() {
        return constant_filter(false);
    }
    format!(
        "file_id IN ({})",
        ids.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn constant_filter(value: bool) -> String {
    // Every stored document has a file ID, including ID zero.
    format!("file_id IS {}NULL", if value { "NOT " } else { "" })
}

fn boolean_filter(
    store: &NativeStore,
    filters: &[StoragePathFilter],
    conjunction: bool,
    negated: bool,
) -> EngineResult<String> {
    if filters.is_empty() {
        return Ok(constant_filter(conjunction));
    }
    let operator = if conjunction { " AND " } else { " OR " };
    Ok(format!(
        "({})",
        filters
            .iter()
            .map(|filter| path_filter(store, filter, negated))
            .collect::<EngineResult<Vec<_>>>()?
            .join(operator)
    ))
}

fn like_literal(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn empty_filter(filter: Option<&StorageSearchFilter>) -> bool {
    filter.is_some_and(|filter| {
        filter.file_ids.as_ref().is_some_and(Vec::is_empty)
            || filter.entity_ids.as_ref().is_some_and(Vec::is_empty)
            || filter.symbol_names.as_ref().is_some_and(Vec::is_empty)
            || filter.symbol_types.as_ref().is_some_and(Vec::is_empty)
    })
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "\\'"))
}

fn top_k(limit: usize) -> EngineResult<i32> {
    if limit > MAX_TOP_K {
        return Err(EngineError::invalid_argument(format!(
            "storage query limit must not exceed {MAX_TOP_K}"
        )));
    }
    i32::try_from(limit)
        .map_err(|_| EngineError::invalid_argument("storage query limit is too large"))
}

fn lexical_text(entity: &Entity, fragment: &EntityFragment) -> EngineResult<String> {
    let metadata = entity.metadata.as_ref();
    let mut output = String::new();
    if let Some(metadata) = metadata {
        match metadata {
            EntityMetadata::Code(code) => {
                for value in [
                    &code.symbol_name,
                    &code.scope,
                    &code.signature,
                    &code.documentation,
                ]
                .into_iter()
                .flatten()
                {
                    output.push_str(value);
                    output.push('\n');
                }
            }
            EntityMetadata::Markdown(markdown) => {
                for value in [&markdown.heading, &markdown.scope].into_iter().flatten() {
                    output.push_str(value);
                    output.push('\n');
                }
            }
        }
    }
    append_contents(
        &mut output,
        std::slice::from_ref(&fragment.range.extract(&entity.content)?),
    );
    Ok(if output.contains('\0') {
        output.replace('\0', " ")
    } else {
        output
    })
}

// C strings cannot contain NUL; indexed projections preserve token boundaries.
fn index_text(text: &str) -> Cow<'_, str> {
    if text.contains('\0') {
        Cow::Owned(text.replace('\0', " "))
    } else {
        Cow::Borrowed(text)
    }
}

fn append_contents(output: &mut String, contents: &[Content]) {
    for content in contents {
        match content {
            Content::Text(text) => output.push_str(text),
            Content::Image(image) => {
                output.push_str("[image:");
                output.push_str(image.format().as_str());
                output.push(']');
            }
            Content::Table(table) => {
                for cell in &table.cells {
                    append_contents(output, &cell.contents);
                }
            }
        }
        output.push('\n');
    }
}

fn entity_key(id: &EntityId) -> String {
    primary_key("entity", id.as_str())
}

fn reject_foreign_ids(
    collection: &Collection,
    keys: &[String],
    file_id: FileId,
) -> EngineResult<()> {
    for doc in fetch_map(collection, keys)?.into_values() {
        if u32_field(&doc, "file_id")? != file_id.get() {
            return Err(EngineError::invalid_argument(
                "entity or fragment ID is already owned by another file",
            ));
        }
    }
    Ok(())
}

/// Every canonical fragment has exactly one projection, and its entity has one model.
pub(super) fn validate_projections(
    entities: &[Entity],
    entries: &[IndexedFragment],
) -> EngineResult<()> {
    let expected = entities
        .iter()
        .flat_map(|entity| {
            entity
                .fragments
                .iter()
                .map(move |fragment| (&fragment.id, &entity.id))
        })
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::new();
    let mut models = HashMap::new();
    for entry in entries {
        if expected.get(&entry.fragment_id).copied() != Some(&entry.entity_id) {
            return Err(EngineError::invalid_argument(
                "search projection does not reference its canonical entity fragment",
            ));
        }
        if !seen.insert(&entry.fragment_id) {
            return Err(EngineError::invalid_argument(
                "canonical fragment has more than one search projection",
            ));
        }
        if models
            .insert(&entry.entity_id, &entry.model)
            .is_some_and(|previous| previous != &entry.model)
        {
            return Err(EngineError::invalid_argument(
                "all fragments of an entity must use one embedding model",
            ));
        }
    }
    if expected.len() != seen.len() {
        return Err(EngineError::invalid_argument(
            "every canonical fragment requires a search projection",
        ));
    }
    Ok(())
}

fn file_key(id: FileId) -> String {
    format!("f{}", id.get())
}

fn u32_field(doc: &Doc, name: &str) -> EngineResult<u32> {
    native(doc.get_u32(name), "read numeric field")?.ok_or_else(|| corrupt("missing numeric field"))
}

fn primary_key(namespace: &str, value: &str) -> String {
    sha256_hex_parts([namespace.as_bytes(), b"\0", value.as_bytes()])
}

fn doc_key(doc: &Doc) -> EngineResult<&str> {
    doc.get_pk()
        .ok_or_else(|| corrupt("stored record has no primary key"))
}
fn string_field(doc: &Doc, field: &str) -> EngineResult<String> {
    native(doc.get_string(field), "read stored field")?
        .ok_or_else(|| corrupt(format!("stored field {field} is missing")))
}
fn corrupt(message: impl std::fmt::Display) -> EngineError {
    EngineError::storage_failure(message.to_string())
}
#[track_caller]
fn native<T>(result: zvec_rust::Result<T>, operation: &str) -> EngineResult<T> {
    result.map_err(|error| EngineError::storage_failure(format!("zvec {operation}: {error}")))
}

#[cfg(test)]
mod tests;
