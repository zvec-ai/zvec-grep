//! Per-model full-text projections in zvec and persistent vectors in `DiskANN`.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt::Write as _,
    path::Path,
};

use zvec_rust::{
    Collection, CollectionSchema, DataType, Doc, FieldSchema, Fts, IndexParams, SearchQuery,
};

use super::{
    path::{file_membership_doc, file_membership_schema},
    types::{
        IndexedFragment, StoragePathFilter, StorageSearchFilter, StorageSearchHit,
        StorageSearchPath,
    },
    zvec::{
        collect_entity_ids, corrupt, doc_key, fetch_map, native, open_collection, quote, scalar,
        string_field, u32_field, write_docs,
    },
};
use crate::{
    EngineError, EngineResult,
    domain::{
        CodeMetadata, DirectoryId, Entity, EntityFragment, EntityId, EntityMetadata, FTS_CONFIG,
        FileId, FileRecord, FragmentId, IndexField, SourcePath,
        model::{EmbeddingMetric, EmbeddingModelInfo},
    },
    utils::sha256_hex_parts,
};

const MAX_TOP_K: usize = 100_000;

pub(super) struct Fragments {
    indexes: BTreeMap<String, Collection>,
    vectors: BTreeMap<String, zg_diskann::VectorIndex>,
}

pub(super) struct PreparedModel {
    docs: Vec<Doc>,
    vectors: Vec<(String, u32, Vec<f32>)>,
}

impl Fragments {
    pub(super) fn open(
        root: &Path,
        embeddings: &[EmbeddingModelInfo],
        read_only: bool,
    ) -> EngineResult<Self> {
        let mut indexes = BTreeMap::new();
        let mut vectors = BTreeMap::new();
        for embedding in embeddings {
            let metric = match embedding.metric() {
                EmbeddingMetric::Cosine => zg_diskann::Distance::Cosine,
                EmbeddingMetric::DotProduct => zg_diskann::Distance::DotProduct,
                EmbeddingMetric::Euclidean => zg_diskann::Distance::Euclidean,
            };
            let name = fragment_collection_name(embedding);
            let collection = open_collection(&root.join(&name), &fragments_schema()?, read_only)?;
            vectors.insert(
                embedding.model().reference(),
                zg_diskann::VectorIndex::open(
                    &root.join(format!("{name}.diskann.sqlite")),
                    embedding.dimension(),
                    metric,
                    read_only,
                )
                .map_err(diskann_error)?,
            );
            if indexes
                .insert(embedding.model().reference(), collection)
                .is_some()
            {
                return Err(EngineError::invalid_argument("duplicate embedding model"));
            }
        }
        Ok(Self { indexes, vectors })
    }

    /// Encode the entire replacement before any stored rows are changed.
    pub(super) fn prepare(
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
        directories: &[DirectoryId],
    ) -> EngineResult<BTreeMap<String, PreparedModel>> {
        let owners = entities
            .iter()
            .map(|entity| {
                let fields = entity
                    .metadata()
                    .map(|metadata| {
                        let value = serde_json::to_value(metadata)
                            .map_err(|error| corrupt(format!("encode entity metadata: {error}")))?;
                        encode_metadata_fields(&value)
                    })
                    .transpose()?
                    .unwrap_or_default();
                Ok((entity.id(), (entity, fields)))
            })
            .collect::<EngineResult<HashMap<_, _>>>()?;
        let fragments = entities
            .iter()
            .flat_map(Entity::fragments)
            .map(|fragment| (&fragment.id, fragment))
            .collect::<HashMap<_, _>>();
        let mut projections: BTreeMap<String, PreparedModel> = BTreeMap::new();
        for entry in entries {
            let (owner, fields) = owners.get(&entry.entity_id).ok_or_else(|| {
                EngineError::invalid_argument("search projection references a missing entity")
            })?;
            let fragment = fragments.get(&entry.fragment_id).ok_or_else(|| {
                EngineError::invalid_argument("search projection references a missing fragment")
            })?;
            let mut doc = fragment_doc(owner, fragment, file, directories, fields)?;
            native(
                doc.add_string("text", &index_text(&entry.fts_text)),
                "encode searchable text",
            )?;
            let projection =
                projections
                    .entry(entry.model.clone())
                    .or_insert_with(|| PreparedModel {
                        docs: Vec::new(),
                        vectors: Vec::new(),
                    });
            projection.docs.push(doc);
            projection.vectors.push((
                entry.fragment_id.as_str().to_owned(),
                file.id().get(),
                entry.vector.clone(),
            ));
        }
        Ok(projections)
    }

    pub(super) fn write(&self, projections: &BTreeMap<String, PreparedModel>) -> EngineResult<()> {
        for (model, projection) in projections {
            let vectors = projection
                .vectors
                .iter()
                .map(|(document_id, file_id, vector)| zg_diskann::Record {
                    document_id,
                    file_id: *file_id,
                    vector,
                })
                .collect::<Vec<_>>();
            self.vector_index(model)?
                .upsert(&vectors)
                .map_err(diskann_error)?;
            write_docs(
                self.index(model)?,
                &projection.docs,
                "write model fragments",
            )?;
        }
        Ok(())
    }

    pub(super) fn validate_ownership(
        &self,
        entries: &[IndexedFragment],
        file_id: FileId,
    ) -> EngineResult<()> {
        let keys = entries
            .iter()
            .map(|entry| entry.fragment_id.as_str().to_owned())
            .collect::<Vec<_>>();
        for index in self.indexes.values() {
            reject_foreign_ids(index, &keys, file_id)?;
        }
        Ok(())
    }

    pub(super) fn delete_file(&self, file_id: FileId) -> EngineResult<()> {
        for index in self.vectors.values() {
            index.delete_file(file_id.get()).map_err(diskann_error)?;
        }
        let filter = format!("file_id = {}", file_id.get());
        for index in self.indexes.values() {
            native(index.delete_by_filter(&filter), "delete source fragments")?;
        }
        Ok(())
    }

    pub(super) fn flush(&self) -> EngineResult<()> {
        for index in self.indexes.values() {
            native(index.flush(), "flush fragment collection")?;
        }
        Ok(())
    }

    pub(super) fn needs_optimize(&self) -> EngineResult<bool> {
        for index in self.indexes.values() {
            let stats = native(index.stats(), "inspect fragment indexes")?;
            if stats.indexes.iter().any(|stat| stat.completeness < 1.0) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn optimize(&self) -> EngineResult<()> {
        for index in self.indexes.values() {
            native(index.optimize(), "optimize fragment collection")?;
        }
        Ok(())
    }

    pub(super) fn search_fts(
        &self,
        query: &str,
        limit: usize,
        filter: Option<&str>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        if limit == 0 {
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
            &mut request,
            filter,
            &["document_id", "entity_id", "file_id"],
        )?;
        let mut hits = Vec::new();
        for index in self.indexes.values() {
            for (rank, doc) in native(index.query(&request), "search full-text index")?
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

    /// Collect all matching entities across model tables, independent of fragment count.
    /// The existing hashed symbol-name projection preserves exact case and literals.
    pub(super) fn find_symbol_entity_ids(
        &self,
        name: &str,
        path_filter: Option<&str>,
    ) -> EngineResult<Vec<EntityId>> {
        let mut filter = metadata_in_filter(CodeMetadata::SYMBOL_NAME, std::iter::once(name));
        if let Some(path_filter) = path_filter {
            filter.push_str(" AND (");
            filter.push_str(path_filter);
            filter.push(')');
        }
        let mut ids = BTreeSet::new();
        for index in self.indexes.values() {
            ids.extend(collect_entity_ids(index, &filter, |_| Ok(()))?);
        }
        Ok(ids.into_iter().map(EntityId::from_string).collect())
    }

    pub(super) fn search_vector(
        &self,
        model: &str,
        vector: &[f32],
        limit: usize,
        filter: Option<&str>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        let index = self.index(model)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        top_k(limit)?;
        let allowed = filter
            .map(|filter| matching_documents(index, filter))
            .transpose()?;
        let hits = self
            .vector_index(model)?
            .search(vector, limit, allowed.as_ref())
            .map_err(diskann_error)?;
        let keys = hits
            .iter()
            .map(|hit| hit.document_id.clone())
            .collect::<Vec<_>>();
        let docs = fetch_map(index, &keys)?;
        hits.into_iter()
            .filter_map(|hit| {
                // A failed cross-store write may leave an unpublished vector. The
                // canonical projection must exist before it can become a result.
                docs.get(&hit.document_id).map(|doc| {
                    let mut decoded = decode_search_hit(doc, StorageSearchPath::Vector)?;
                    decoded.score = hit.score;
                    Ok(decoded)
                })
            })
            .collect()
    }

    fn vector_index(&self, model: &str) -> EngineResult<&zg_diskann::VectorIndex> {
        self.vectors.get(model).ok_or_else(|| {
            EngineError::invalid_argument(format!("unknown embedding model {model:?}"))
        })
    }

    fn index(&self, model: &str) -> EngineResult<&Collection> {
        self.indexes.get(model).ok_or_else(|| {
            EngineError::invalid_argument(format!("unknown embedding model {model:?}"))
        })
    }

    #[cfg(test)]
    pub(super) fn collection(&self, model: &str) -> EngineResult<&Collection> {
        self.index(model)
    }
}

fn primary_key(namespace: &str, value: &str) -> String {
    sha256_hex_parts([namespace.as_bytes(), b"\0", value.as_bytes()])
}

pub(super) fn fragment_collection_name(embedding: &EmbeddingModelInfo) -> String {
    format!(
        "fragments_{:.4}",
        primary_key(
            "space",
            &format!(
                "{}\0{}\0{}\0{:?}",
                embedding.model().provider(),
                embedding.model().name(),
                embedding.dimension(),
                embedding.metric()
            )
        )
    )
}

fn identity_schema(name: &str) -> EngineResult<CollectionSchema> {
    let mut schema = native(CollectionSchema::new(name), "create collection schema")?;
    scalar(&mut schema, "file_id", DataType::Uint32, false, true)?;
    scalar(&mut schema, "entity_id", DataType::String, false, true)?;
    scalar(&mut schema, "document_id", DataType::String, false, true)?;
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

fn fragments_schema() -> EngineResult<CollectionSchema> {
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

    Ok(schema)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "map_err consumes the storage error"
)]
fn diskann_error(error: zg_diskann::Error) -> EngineError {
    EngineError::storage_failure(format!("DiskANN storage: {error}"))
}

/// Partition full scalar batches around a collected ID, avoiding any result cap
/// or dependence on zvec's unspecified scalar-result order.
fn matching_documents(index: &Collection, filter: &str) -> EngineResult<HashSet<String>> {
    let mut ids = HashSet::new();
    let mut ranges: Vec<(Option<String>, Option<String>)> = vec![(None, None)];
    while let Some((lower, upper)) = ranges.pop() {
        let mut clause = if lower.is_none() && upper.is_none() {
            filter.to_owned()
        } else {
            format!("({filter})")
        };
        if let Some(lower) = &lower {
            let _ = write!(clause, " AND document_id > {}", quote(lower));
        }
        if let Some(upper) = &upper {
            let _ = write!(clause, " AND document_id < {}", quote(upper));
        }
        let mut query = native(SearchQuery::scalar(1024), "create vector membership query")?;
        configure_query(&mut query, Some(&clause), &["document_id"])?;
        let docs = native(index.query(&query), "resolve vector membership")?;
        let full = docs.len() == 1024;
        let mut batch = docs
            .iter()
            .map(|doc| string_field(doc, "document_id"))
            .collect::<EngineResult<Vec<_>>>()?;
        batch.sort();
        batch.dedup();
        if batch.iter().any(|id| {
            lower.as_ref().is_some_and(|lo| id <= lo) || upper.as_ref().is_some_and(|hi| id >= hi)
        }) {
            return Err(corrupt(
                "vector membership returned an ID outside its range",
            ));
        }
        if full {
            let pivot = batch[batch.len() / 2].clone();
            ranges.push((Some(pivot.clone()), upper));
            ranges.push((lower, Some(pivot)));
        }
        ids.extend(batch);
    }
    Ok(ids)
}

fn identity_doc(entity: &Entity, fragment: &EntityFragment) -> EngineResult<Doc> {
    let mut doc = native(Doc::new(), "create fragment record")?;
    doc.set_pk(fragment.id.as_str());
    native(
        doc.add_u32("file_id", entity.file_id().get()),
        "encode source identity",
    )?;
    native(
        doc.add_string("entity_id", entity.id().as_str()),
        "encode entity identity",
    )?;
    native(
        doc.add_string("document_id", fragment.id.as_str()),
        "encode document identity",
    )?;
    Ok(doc)
}

fn fragment_doc(
    entity: &Entity,
    fragment: &EntityFragment,
    file: &FileRecord,
    directories: &[DirectoryId],
    fields: &[(IndexField, String)],
) -> EngineResult<Doc> {
    let mut doc = identity_doc(entity, fragment)?;
    file_membership_doc(&mut doc, file, directories)?;
    for (field, value) in fields {
        native(
            doc.add_string(field.name(), value),
            "encode metadata index field",
        )?;
    }
    Ok(doc)
}

fn decode_search_hit(doc: &Doc, path: StorageSearchPath) -> EngineResult<StorageSearchHit> {
    let document_id = FragmentId::from_string(string_field(doc, "document_id")?);
    let entity_id = EntityId::from_string(string_field(doc, "entity_id")?);
    if doc_key(doc)? != document_id.as_str() {
        return Err(corrupt(
            "search document identity differs from its primary key",
        ));
    }
    Ok(StorageSearchHit {
        document_id: document_id.as_str().to_owned(),
        entity_id,
        file_id: FileId::new(u32_field(doc, "file_id")?),
        path,
        score: f64::from(doc.get_score()),
    })
}

fn configure_query(
    query: &mut SearchQuery,
    filter: Option<&str>,
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
    if let Some(filter) = filter {
        native(query.set_filter(filter), "set search filter")?;
    }
    Ok(())
}

pub(super) fn build_filter(
    filter: Option<&StorageSearchFilter>,
    resolve_directory: &impl Fn(&SourcePath) -> EngineResult<Option<DirectoryId>>,
) -> EngineResult<Option<String>> {
    let Some(filter) = filter else {
        return Ok(None);
    };
    if empty_filter(Some(filter)) {
        return Ok(Some(constant_filter(false)));
    }
    let mut clauses = Vec::new();
    if let Some(path) = &filter.path
        && !matches!(path, StoragePathFilter::All)
    {
        clauses.push(path_filter(resolve_directory, path, false)?);
    }
    if let Some(ids) = &filter.file_ids {
        clauses.push(file_id_filter(ids));
    }
    if let Some(ids) = &filter.entity_ids {
        clauses.push(in_filter("entity_id", ids.iter().map(EntityId::as_str)));
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
    resolve_directory: &impl Fn(&SourcePath) -> EngineResult<Option<DirectoryId>>,
    filter: &StoragePathFilter,
    negated: bool,
) -> EngineResult<String> {
    Ok(match filter {
        StoragePathFilter::All => constant_filter(!negated),
        StoragePathFilter::None => constant_filter(negated),
        StoragePathFilter::Directory(path) => {
            let Some(id) = resolve_directory(path)? else {
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
        StoragePathFilter::And(filters) => {
            boolean_filter(resolve_directory, filters, !negated, negated)?
        }
        StoragePathFilter::Or(filters) => {
            boolean_filter(resolve_directory, filters, negated, negated)?
        }
        StoragePathFilter::Not(filter) => path_filter(resolve_directory, filter, !negated)?,
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
    resolve_directory: &impl Fn(&SourcePath) -> EngineResult<Option<DirectoryId>>,
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
            .map(|filter| path_filter(resolve_directory, filter, negated))
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

pub(super) fn empty_filter(filter: Option<&StorageSearchFilter>) -> bool {
    filter.is_some_and(|filter| {
        filter.file_ids.as_ref().is_some_and(Vec::is_empty)
            || filter.entity_ids.as_ref().is_some_and(Vec::is_empty)
            || filter.symbol_names.as_ref().is_some_and(Vec::is_empty)
            || filter.symbol_types.as_ref().is_some_and(Vec::is_empty)
    })
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

// C strings cannot contain NUL; indexed projections preserve token boundaries.
fn index_text(text: &str) -> Cow<'_, str> {
    if text.contains('\0') {
        Cow::Owned(text.replace('\0', " "))
    } else {
        Cow::Borrowed(text)
    }
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
                .fragments()
                .iter()
                .map(move |fragment| (&fragment.id, entity.id()))
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

#[cfg(test)]
mod tests;
