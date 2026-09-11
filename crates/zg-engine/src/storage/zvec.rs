use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::Path,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zvec_rust::{
    Collection, CollectionOptions, CollectionSchema, DataType, Doc, FieldSchema, Fts, IndexParams,
    MetricType, SearchQuery,
};

use super::{
    codec,
    spi::{
        FileIndexStatus, IndexedFragment, StorageSearchFilter, StorageSearchHit, StorageSearchPath,
        StoredEntity, StoredFile, WorkspaceIndexEmbeddingSchema,
    },
};
use crate::{
    EngineError, EngineResult,
    domain::{
        Content, EntityContent, EntityFragment, EntityId, EntityMetadata, FileId, SymbolType,
        validate_fragments,
    },
    models::EmbeddingMetric,
};

const WRITE_BATCH: usize = 1024;
const MAX_TOP_K: usize = 100_000;

pub(super) struct NativeStore {
    files: Collection,
    entities: Collection,
    fragments: Collection,
    vectors: Collection,
    dimension: usize,
    read_only: bool,
}

impl NativeStore {
    pub(super) fn open(
        path: &Path,
        embedding: &WorkspaceIndexEmbeddingSchema,
        read_only: bool,
    ) -> EngineResult<Self> {
        let dimension = u32::try_from(embedding.dimension)
            .ok()
            .filter(|value| (1..=20_000).contains(value))
            .ok_or_else(|| {
                EngineError::invalid_argument(
                    "storage embedding dimension must be between 1 and 20,000",
                )
            })?;
        let dictionary = path.join("dictionary");
        let dictionary = dictionary
            .to_str()
            .ok_or_else(|| EngineError::invalid_argument("zvec dictionary path must be UTF-8"))?;
        let params = serde_json::json!({"jieba_dict_dir": dictionary}).to_string();
        let files = open_collection(&path.join("files"), &files_schema()?, read_only)?;
        let entities = open_collection(&path.join("entities"), &entities_schema()?, read_only)?;
        let fragments = open_collection(
            &path.join("fragments"),
            &fragments_schema(&params)?,
            read_only,
        )?;
        let metric = match embedding.metric {
            EmbeddingMetric::Cosine => MetricType::Cosine,
            EmbeddingMetric::DotProduct => MetricType::Ip,
            EmbeddingMetric::Euclidean => MetricType::L2,
        };
        let space = vector_collection_name(embedding);
        let vectors = open_collection(
            &path.join(space),
            &vectors_schema(dimension, metric)?,
            read_only,
        )?;
        Ok(Self {
            files,
            entities,
            fragments,
            vectors,
            dimension: embedding.dimension,
            read_only,
        })
    }

    pub(super) fn list_files(&self) -> EngineResult<Vec<StoredFile>> {
        let iterator = native(
            self.files.iter_with_options(None, false),
            "iterate source files",
        )?;
        let mut files = iterator
            .map(|doc| decode_file_doc(&native(doc, "read source file")?))
            .collect::<EngineResult<Vec<_>>>()?;
        files.sort_by(|left, right| {
            left.source
                .relative_path
                .cmp(&right.source.relative_path)
                .then_with(|| left.source.id.as_str().cmp(right.source.id.as_str()))
        });
        Ok(files)
    }

    pub(super) fn get_file(&self, id: &FileId) -> EngineResult<Option<StoredFile>> {
        let key = primary_key("file", id.as_str());
        fetch_one(&self.files, &key)?
            .as_ref()
            .map(decode_file_doc)
            .transpose()
    }

    pub(super) fn get_entity(&self, id: &EntityId) -> EngineResult<Option<StoredEntity>> {
        let key = primary_key("fragment", id.as_str());
        let Some(doc) = fetch_one(&self.entities, &key)? else {
            return Ok(None);
        };
        let fragment = decode_fragment_doc(&doc)?;
        let entity = fragment
            .as_entity()
            .ok_or_else(|| corrupt("window found in entity collection"))?
            .clone();
        if entity.id != *id {
            return Err(corrupt("entity ID does not match its primary key"));
        }
        let file = self
            .get_file(&entity.file_id)?
            .ok_or_else(|| corrupt("entity references a missing file"))?;
        Ok(Some(StoredEntity { entity, file }))
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
        configure_query(&mut request, filter, &["payload", "source_id", "entity_id"])?;
        let docs = native(self.fragments.query(&request), "search full-text index")?;
        self.hydrate(
            docs.into_iter()
                .map(|doc| {
                    let score = f64::from(doc.get_score());
                    (doc, score)
                })
                .collect(),
            StorageSearchPath::Fts,
        )
    }

    pub(super) fn search_vector(
        &self,
        vector: &[f32],
        limit: usize,
        filter: Option<&StorageSearchFilter>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        self.validate_vector(vector)?;
        if limit == 0 || empty_filter(filter) {
            return Ok(Vec::new());
        }
        let mut request = native(
            SearchQuery::new("embedding", vector, top_k(limit)?),
            "create vector query",
        )?;
        configure_query(&mut request, filter, &["fragment_id"])?;
        let ranked = native(self.vectors.query(&request), "search vector index")?
            .into_iter()
            .map(|doc| {
                Ok((
                    string_field(&doc, "fragment_id")?,
                    f64::from(doc.get_score()),
                ))
            })
            .collect::<EngineResult<Vec<_>>>()?;
        let mut docs = fetch_map(
            &self.fragments,
            &ranked.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        )?;
        let fragments = ranked
            .into_iter()
            .map(|(key, score)| {
                Ok((
                    docs.remove(&key)
                        .ok_or_else(|| corrupt("vector references a missing fragment"))?,
                    score,
                ))
            })
            .collect::<EngineResult<Vec<_>>>()?;
        self.hydrate(fragments, StorageSearchPath::Vector)
    }

    fn hydrate(
        &self,
        docs: Vec<(Doc, f64)>,
        path: StorageSearchPath,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        let fragments = docs
            .into_iter()
            .map(|(doc, score)| Ok((decode_fragment_doc(&doc)?, score)))
            .collect::<EngineResult<Vec<_>>>()?;
        let keys = fragments
            .iter()
            .map(|(fragment, _)| primary_key("file", fragment.file_id().as_str()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let files = fetch_map(&self.files, &keys)?
            .into_iter()
            .map(|(key, doc)| Ok((key, decode_file_doc(&doc)?)))
            .collect::<EngineResult<HashMap<_, _>>>()?;
        fragments
            .into_iter()
            .map(|(fragment, score)| {
                let file = files
                    .get(&primary_key("file", fragment.file_id().as_str()))
                    .ok_or_else(|| corrupt("fragment references a missing file"))?
                    .clone();
                Ok(StorageSearchHit {
                    fragment,
                    file,
                    path,
                    score,
                })
            })
            .collect()
    }

    pub(super) fn apply_replace(
        &self,
        file: &StoredFile,
        entries: &[IndexedFragment],
    ) -> EngineResult<()> {
        self.assert_writable()?;
        validate_fragments(&file.source.id, entries.iter().map(|entry| &entry.fragment))?;
        let file_doc = encode_file_doc(file)?;
        let mut fragments = Vec::with_capacity(entries.len());
        let mut entities = Vec::new();
        let mut vectors = Vec::with_capacity(entries.len());
        for entry in entries {
            self.validate_vector(&entry.vector)?;
            let payload = codec::encode_fragment(&entry.fragment)?;
            let mut fragment = fragment_doc(&entry.fragment)?;
            native(
                fragment.add_string("payload", &payload),
                "encode fragment payload",
            )?;
            native(
                fragment.add_string("text", &lexical_text(&entry.fragment)),
                "encode searchable text",
            )?;
            fragments.push(fragment);
            if entry.fragment.as_entity().is_some() {
                let mut entity = identity_doc(&entry.fragment)?;
                native(
                    entity.add_string("payload", &payload),
                    "encode entity payload",
                )?;
                entities.push(entity);
            }
            let mut vector = fragment_doc(&entry.fragment)?;
            native(
                vector.add_string(
                    "fragment_id",
                    &primary_key("fragment", entry.fragment.document_id()),
                ),
                "encode vector owner",
            )?;
            native(
                vector.add_vector_f32("embedding", &entry.vector),
                "encode embedding vector",
            )?;
            vectors.push(vector);
        }
        self.delete_documents(&file.source.id)?;
        write_docs(&self.entities, &entities, "write entities")?;
        write_docs(&self.fragments, &fragments, "write fragments")?;
        write_docs(&self.vectors, &vectors, "write vectors")?;
        write_docs(&self.files, &[file_doc], "publish source file")
    }

    pub(super) fn apply_delete(&self, id: &FileId) -> EngineResult<()> {
        self.assert_writable()?;
        self.delete_documents(id)?;
        native(
            self.files.delete_by_filter(&format!(
                "source_id = {}",
                quote(&primary_key("file", id.as_str()))
            )),
            "delete source file",
        )
    }

    fn delete_documents(&self, id: &FileId) -> EngineResult<()> {
        let filter = format!("source_id = {}", quote(&primary_key("file", id.as_str())));
        for (collection, operation) in [
            (&self.vectors, "delete source vectors"),
            (&self.fragments, "delete source fragments"),
            (&self.entities, "delete source entities"),
        ] {
            native(collection.delete_by_filter(&filter), operation)?;
        }
        Ok(())
    }

    pub(super) fn flush(&self) -> EngineResult<()> {
        self.assert_writable()?;
        for collection in [&self.entities, &self.fragments, &self.vectors, &self.files] {
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

    fn validate_vector(&self, vector: &[f32]) -> EngineResult<()> {
        if vector.len() != self.dimension || vector.iter().any(|value| !value.is_finite()) {
            return Err(EngineError::invalid_argument(format!(
                "expected {} finite embedding values, got {}",
                self.dimension,
                vector.len()
            )));
        }
        Ok(())
    }
}

pub(super) fn vector_collection_name(embedding: &WorkspaceIndexEmbeddingSchema) -> String {
    format!(
        "vectors_{}",
        primary_key(
            "space",
            &format!(
                "{}\0{}\0{}\0{:?}",
                embedding.provider, embedding.model, embedding.dimension, embedding.metric
            )
        )
    )
}

fn open_collection(
    path: &Path,
    schema: &CollectionSchema,
    read_only: bool,
) -> EngineResult<Collection> {
    let text = path.to_str().ok_or_else(|| {
        EngineError::invalid_argument(format!(
            "zvec storage path must be UTF-8: {}",
            path.display()
        ))
    })?;
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
    scalar(&mut schema, "source_id", DataType::String, false, true)?;
    scalar(&mut schema, "entity_id", DataType::String, false, true)?;
    Ok(schema)
}

fn retrieval_schema(name: &str) -> EngineResult<CollectionSchema> {
    let mut schema = identity_schema(name)?;
    scalar(&mut schema, "symbol_name", DataType::String, true, true)?;
    scalar(&mut schema, "symbol_type", DataType::String, true, true)?;
    Ok(schema)
}

fn files_schema() -> EngineResult<CollectionSchema> {
    let mut schema = native(CollectionSchema::new("files"), "create files schema")?;
    scalar(&mut schema, "source_id", DataType::String, false, true)?;
    scalar(&mut schema, "payload", DataType::String, false, false)?;
    scalar(&mut schema, "status", DataType::String, false, false)?;
    Ok(schema)
}

fn entities_schema() -> EngineResult<CollectionSchema> {
    let mut schema = identity_schema("entities")?;
    scalar(&mut schema, "payload", DataType::String, false, false)?;
    Ok(schema)
}

fn fragments_schema(params: &str) -> EngineResult<CollectionSchema> {
    let mut schema = retrieval_schema("fragments")?;
    scalar(&mut schema, "payload", DataType::String, false, false)?;
    let mut text = native(
        FieldSchema::new("text", DataType::String, false, 0),
        "define full-text field",
    )?;
    native(
        text.set_index_params(&native(
            IndexParams::fts(Some("jieba"), Some(&["lowercase"]), Some(params)),
            "define full-text index",
        )?),
        "attach full-text index",
    )?;
    native(schema.add_field(&text), "add full-text field")?;
    Ok(schema)
}

fn vectors_schema(dimension: u32, metric: MetricType) -> EngineResult<CollectionSchema> {
    let mut schema = retrieval_schema("vectors")?;
    scalar(&mut schema, "fragment_id", DataType::String, false, false)?;
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

#[derive(Serialize, Deserialize)]
struct StatusRecord {
    version: u8,
    status: Option<IndexStatusRecord>,
}

#[derive(Serialize, Deserialize)]
struct IndexStatusRecord {
    indexed_epoch_ms: Option<u64>,
    entity_count: usize,
    token_count: Option<usize>,
    truncated_fragment_count: Option<usize>,
    error: Option<String>,
}

fn encode_file_doc(file: &StoredFile) -> EngineResult<Doc> {
    let mut doc = native(Doc::new(), "create file record")?;
    let key = primary_key("file", file.source.id.as_str());
    doc.set_pk(&key);
    native(doc.add_string("source_id", &key), "encode file identity")?;
    native(
        doc.add_string("payload", &codec::encode_file(&file.source)?),
        "encode source payload",
    )?;
    let status = StatusRecord {
        version: 1,
        status: file.index_status.as_ref().map(|status| IndexStatusRecord {
            indexed_epoch_ms: status.indexed_epoch_ms,
            entity_count: status.entity_count,
            token_count: status.token_count,
            truncated_fragment_count: status.truncated_fragment_count,
            error: status.error.clone(),
        }),
    };
    let status = serde_json::to_string(&status)
        .map_err(|error| corrupt(&format!("cannot encode file status: {error}")))?;
    native(doc.add_string("status", &status), "encode file status")?;
    Ok(doc)
}

fn decode_file_doc(doc: &Doc) -> EngineResult<StoredFile> {
    let source = codec::decode_file(&string_field(doc, "payload")?)?;
    if doc_key(doc)? != primary_key("file", source.id.as_str())
        || string_field(doc, "source_id")? != doc_key(doc)?
    {
        return Err(corrupt("file identity differs from its primary key"));
    }
    let record: StatusRecord = serde_json::from_str(&string_field(doc, "status")?)
        .map_err(|error| corrupt(&format!("cannot decode file status: {error}")))?;
    if record.version != 1 {
        return Err(corrupt("unsupported file status version"));
    }
    let index_status = record.status.map(|status| FileIndexStatus {
        indexed_epoch_ms: status.indexed_epoch_ms,
        entity_count: status.entity_count,
        token_count: status.token_count,
        truncated_fragment_count: status.truncated_fragment_count,
        error: status.error,
    });
    Ok(StoredFile {
        source,
        index_status,
    })
}

fn identity_doc(fragment: &EntityFragment) -> EngineResult<Doc> {
    let mut doc = native(Doc::new(), "create fragment record")?;
    doc.set_pk(&primary_key("fragment", fragment.document_id()));
    native(
        doc.add_string(
            "source_id",
            &primary_key("file", fragment.file_id().as_str()),
        ),
        "encode source identity",
    )?;
    native(
        doc.add_string(
            "entity_id",
            &primary_key("fragment", fragment.entity_id().as_str()),
        ),
        "encode entity identity",
    )?;
    Ok(doc)
}

fn fragment_doc(fragment: &EntityFragment) -> EngineResult<Doc> {
    let mut doc = identity_doc(fragment)?;
    if let Some(EntityMetadata::Code {
        symbol_type,
        symbol_name,
        ..
    }) = fragment.metadata()
    {
        native(
            doc.add_string("symbol_type", symbol_type_name(*symbol_type)),
            "encode symbol type",
        )?;
        if let Some(name) = symbol_name {
            native(
                doc.add_string("symbol_name", &primary_key("symbol", name)),
                "encode symbol name",
            )?;
        }
    }
    Ok(doc)
}

fn decode_fragment_doc(doc: &Doc) -> EngineResult<EntityFragment> {
    let fragment = codec::decode_fragment(&string_field(doc, "payload")?)?;
    if doc_key(doc)? != primary_key("fragment", fragment.document_id())
        || string_field(doc, "source_id")? != primary_key("file", fragment.file_id().as_str())
        || string_field(doc, "entity_id")? != primary_key("fragment", fragment.entity_id().as_str())
    {
        return Err(corrupt("fragment identity differs from its index fields"));
    }
    Ok(fragment)
}

fn fetch_one(collection: &Collection, key: &str) -> EngineResult<Option<Doc>> {
    let mut docs = native(
        collection.fetch_with_options(&[key], None, false),
        "fetch stored record",
    )?;
    if docs.len() > 1 {
        return Err(corrupt("primary key returned multiple records"));
    }
    Ok(docs.pop())
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
    if let Some(filter) = build_filter(filter) {
        native(query.set_filter(&filter), "set search filter")?;
    }
    Ok(())
}

fn build_filter(filter: Option<&StorageSearchFilter>) -> Option<String> {
    let filter = filter?;
    let mut clauses = Vec::new();
    if let Some(ids) = &filter.file_ids {
        clauses.push(in_filter(
            "source_id",
            ids.iter().map(|id| primary_key("file", id.as_str())),
        ));
    }
    if let Some(ids) = &filter.entity_ids {
        clauses.push(in_filter(
            "entity_id",
            ids.iter().map(|id| primary_key("fragment", id.as_str())),
        ));
    }
    if let Some(names) = &filter.symbol_names {
        clauses.push(in_filter(
            "symbol_name",
            names.iter().map(|name| primary_key("symbol", name)),
        ));
    }
    if let Some(types) = &filter.symbol_types {
        clauses.push(in_filter(
            "symbol_type",
            types.iter().map(|kind| symbol_type_name(*kind).to_owned()),
        ));
    }
    (!clauses.is_empty()).then(|| clauses.join(" AND "))
}

fn in_filter(field: &str, values: impl Iterator<Item = String>) -> String {
    format!(
        "{field} IN ({})",
        values
            .map(|value| quote(&value))
            .collect::<Vec<_>>()
            .join(", ")
    )
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
    // Filters contain only hashed identities or canonical enum names.
    format!("'{value}'")
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

fn lexical_text(fragment: &EntityFragment) -> String {
    let mut output = String::new();
    if let Some(metadata) = fragment.metadata() {
        match metadata {
            EntityMetadata::Code {
                symbol_name,
                scope,
                signature,
                documentation,
                ..
            } => {
                for value in [symbol_name, scope, signature, documentation]
                    .into_iter()
                    .flatten()
                {
                    output.push_str(value);
                    output.push('\n');
                }
            }
            EntityMetadata::Markdown { heading, scope, .. } => {
                for value in [heading, scope].into_iter().flatten() {
                    output.push_str(value);
                    output.push('\n');
                }
            }
        }
    }
    match fragment {
        EntityFragment::Standalone(entity) | EntityFragment::Representative(entity) => {
            match &entity.content {
                EntityContent::Source(contents) => append_contents(&mut output, contents),
                EntityContent::Outline(text) => output.push_str(text),
            }
        }
        EntityFragment::Window(window) => append_contents(&mut output, &window.contents),
    }
    if output.contains('\0') {
        output.replace('\0', " ")
    } else {
        output
    }
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

fn symbol_type_name(value: SymbolType) -> &'static str {
    match value {
        SymbolType::Module => "module",
        SymbolType::Class => "class",
        SymbolType::Interface => "interface",
        SymbolType::Function => "function",
        SymbolType::Value => "value",
        SymbolType::Alias => "alias",
    }
}

fn primary_key(namespace: &str, value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut digest = Sha256::new();
    digest.update(namespace.as_bytes());
    digest.update([0]);
    digest.update(value.as_bytes());
    let mut output = String::with_capacity(64);
    for byte in digest.finalize() {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 15)]));
    }
    output
}

fn doc_key(doc: &Doc) -> EngineResult<&str> {
    doc.get_pk()
        .ok_or_else(|| corrupt("stored record has no primary key"))
}
fn string_field(doc: &Doc, field: &str) -> EngineResult<String> {
    native(doc.get_string(field), "read stored field")?
        .ok_or_else(|| corrupt(&format!("stored field {field} is missing")))
}
fn corrupt(message: &str) -> EngineError {
    EngineError::storage_failure(message)
}
#[track_caller]
fn native<T>(result: zvec_rust::Result<T>, operation: &str) -> EngineResult<T> {
    result.map_err(|error| EngineError::storage_failure(format!("zvec {operation}: {error}")))
}
