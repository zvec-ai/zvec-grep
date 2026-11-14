//! Search projections: one zvec collection per embedding model, with FTS and FP16 vectors.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::Path,
};

use zvec_rust::{
    Collection, CollectionSchema, DataType, Doc, FieldSchema, Fts, IndexParams, MetricType,
    SearchQuery,
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
        model::{EmbeddingMetric, EmbeddingModelInfo, VectorIndexQuantization},
    },
    utils::sha256_hex_parts,
};

const MAX_TOP_K: usize = 100_000;
const BINARY_SCAN_BUCKETS: u32 = u16::MAX as u32 + 1;
const BINARY_SCAN_TARGET_HITS: usize = 5_000;
const BINARY_SCAN_MAX_INITIAL_RANGES: usize = 256;
const BINARY_SCAN_PAGE_SIZE: usize = 10_000;

pub(super) struct Fragments {
    indexes: BTreeMap<String, Collection>,
    embeddings: BTreeMap<String, EmbeddingModelInfo>,
}

impl Fragments {
    pub(super) fn open(
        root: &Path,
        embeddings: &[EmbeddingModelInfo],
        read_only: bool,
    ) -> EngineResult<Self> {
        let mut indexes = BTreeMap::new();
        let mut stored_embeddings = BTreeMap::new();
        for embedding in embeddings {
            let dimension = u32::try_from(embedding.dimension())
                .ok()
                .filter(|value| (1..=20_000).contains(value))
                .ok_or_else(|| {
                    EngineError::invalid_argument(
                        "storage embedding dimension must be between 1 and 20,000",
                    )
                })?;
            let metric = match embedding.metric() {
                EmbeddingMetric::Cosine => MetricType::Cosine,
                EmbeddingMetric::DotProduct => MetricType::Ip,
                EmbeddingMetric::Euclidean => MetricType::L2,
            };
            let name = fragment_collection_name(embedding);
            let collection = open_collection(
                &root.join(&name),
                &fragments_schema(dimension, metric, embedding.vector_index_quantization())?,
                read_only,
            )?;
            let model_reference = embedding.model().reference();
            if indexes
                .insert(model_reference.clone(), collection)
                .is_some()
            {
                return Err(EngineError::invalid_argument("duplicate embedding model"));
            }
            stored_embeddings.insert(model_reference, embedding.clone());
        }
        Ok(Self {
            indexes,
            embeddings: stored_embeddings,
        })
    }

    /// Encode the entire replacement before any stored rows are changed.
    pub(super) fn prepare(
        file: &FileRecord,
        entities: &[Entity],
        entries: &[IndexedFragment],
        directories: &[DirectoryId],
        embeddings: &BTreeMap<String, EmbeddingModelInfo>,
    ) -> EngineResult<BTreeMap<String, Vec<Doc>>> {
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
        let mut projections: BTreeMap<String, Vec<Doc>> = BTreeMap::new();
        for entry in entries {
            let (owner, fields) = owners.get(&entry.entity_id).ok_or_else(|| {
                EngineError::invalid_argument("search projection references a missing entity")
            })?;
            let fragment = fragments.get(&entry.fragment_id).ok_or_else(|| {
                EngineError::invalid_argument("search projection references a missing fragment")
            })?;
            let embedding = embeddings.get(&entry.model).ok_or_else(|| {
                EngineError::invalid_argument("search projection references an unknown model")
            })?;
            if entry.vector.len() != embedding.dimension() {
                return Err(EngineError::invalid_argument(format!(
                    "embedding vector has {} values; model {} requires {}",
                    entry.vector.len(),
                    entry.model,
                    embedding.dimension()
                )));
            }
            let mut doc = fragment_doc(owner, fragment, file, directories, fields)?;
            native(
                doc.add_string("text", &index_text(&entry.fts_text)),
                "encode searchable text",
            )?;
            match embedding.vector_index_quantization() {
                VectorIndexQuantization::Fp16 => native(
                    zg_zvec_fp16::add_embedding(&mut doc, "embedding", &entry.vector),
                    "encode embedding vector",
                )?,
                VectorIndexQuantization::BinarySign => {
                    let (packed, scale) = encode_sign_vector(&entry.vector)?;
                    native(
                        doc.add_binary("embedding", &packed),
                        "encode packed sign embedding",
                    )?;
                    native(
                        doc.add_f32("embedding_scale", scale),
                        "encode embedding scale",
                    )?;
                    native(
                        doc.add_u32(
                            "vector_scan_bucket",
                            vector_scan_bucket(entry.fragment_id.as_str())?,
                        ),
                        "encode vector scan bucket",
                    )?;
                }
            }
            projections
                .entry(entry.model.clone())
                .or_default()
                .push(doc);
        }
        Ok(projections)
    }

    pub(super) fn write(&self, projections: &BTreeMap<String, Vec<Doc>>) -> EngineResult<()> {
        for (model, docs) in projections {
            write_docs(self.index(model)?, docs, "write model fragments")?;
        }
        Ok(())
    }

    pub(super) fn embedding_models(&self) -> &BTreeMap<String, EmbeddingModelInfo> {
        &self.embeddings
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
        let filter = format!("file_id = {}", file_id.get());
        for index in self.indexes.values() {
            native(index.delete_by_filter(&filter), "delete source fragments")?;
        }
        Ok(())
    }

    /// Count live search projections without loading text or embedding vectors.
    pub(super) fn count(&self) -> EngineResult<u64> {
        let mut count = 0_u64;
        for index in self.indexes.values() {
            let iterator = native(
                index.iter_with_options(Some(&["document_id"]), false),
                "iterate fragment identities",
            )?;
            for doc in iterator {
                native(doc, "read fragment identity")?;
                count = count
                    .checked_add(1)
                    .ok_or_else(|| EngineError::storage_failure("fragment count exceeds u64"))?;
            }
        }
        Ok(count)
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
        let requested_top_k = top_k(limit)?;
        let embedding = self.embeddings.get(model).ok_or_else(|| {
            EngineError::invalid_argument(format!("unknown embedding model {model:?}"))
        })?;
        if embedding.vector_index_quantization() == VectorIndexQuantization::BinarySign {
            return self.search_binary_sign(index, embedding, vector, limit, filter);
        }
        let mut request = native(
            zg_zvec_fp16::embedding_query("embedding", vector, requested_top_k),
            "create vector query",
        )?;
        configure_query(
            &mut request,
            filter,
            &["document_id", "entity_id", "file_id"],
        )?;
        native(index.query(&request), "search vector index")?
            .into_iter()
            .map(|doc| decode_search_hit(&doc, StorageSearchPath::Vector))
            .collect()
    }

    fn search_binary_sign(
        &self,
        index: &Collection,
        embedding: &EmbeddingModelInfo,
        query: &[f32],
        limit: usize,
        filter: Option<&str>,
    ) -> EngineResult<Vec<StorageSearchHit>> {
        if query.len() != embedding.dimension() || query.iter().any(|value| !value.is_finite()) {
            return Err(EngineError::invalid_argument(
                "binary-sign query has an invalid dimension or non-finite value",
            ));
        }
        if embedding.metric() == EmbeddingMetric::Cosine && query.iter().all(|value| *value == 0.0)
        {
            return Err(EngineError::invalid_argument(
                "cosine query vector must not be zero",
            ));
        }

        let stats = native(index.stats(), "inspect binary-vector collection")?;
        let document_count = usize::try_from(stats.doc_count)
            .map_err(|_| EngineError::storage_failure("binary vector count exceeds usize"))?;
        if document_count == 0 {
            return Ok(Vec::new());
        }
        let initial_ranges = document_count
            .div_ceil(BINARY_SCAN_TARGET_HITS)
            .clamp(1, BINARY_SCAN_MAX_INITIAL_RANGES);
        let bucket_width = usize::try_from(BINARY_SCAN_BUCKETS)
            .expect("bucket count fits usize")
            .div_ceil(initial_ranges);
        let mut hits = Vec::with_capacity(limit.min(document_count));
        for start in (0..BINARY_SCAN_BUCKETS).step_by(bucket_width) {
            let end = start
                .saturating_add(u32::try_from(bucket_width).unwrap_or(u32::MAX))
                .min(BINARY_SCAN_BUCKETS);
            self.scan_binary_sign_range(
                index, embedding, query, limit, filter, start, end, &mut hits,
            )?;
        }
        sort_search_hits(&mut hits);
        hits.truncate(limit);
        Ok(hits)
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_binary_sign_range(
        &self,
        index: &Collection,
        embedding: &EmbeddingModelInfo,
        query: &[f32],
        limit: usize,
        filter: Option<&str>,
        bucket_start: u32,
        bucket_end: u32,
        top_hits: &mut Vec<StorageSearchHit>,
    ) -> EngineResult<()> {
        let bucket_filter =
            format!("vector_scan_bucket >= {bucket_start} AND vector_scan_bucket < {bucket_end}");
        let filter = filter.map_or_else(
            || bucket_filter.clone(),
            |filter| format!("({filter}) AND ({bucket_filter})"),
        );
        let mut request = native(
            SearchQuery::scalar(
                i32::try_from(BINARY_SCAN_PAGE_SIZE + 1)
                    .map_err(|_| EngineError::internal("binary scan page size exceeds i32"))?,
            ),
            "create binary-vector scan query",
        )?;
        configure_query(
            &mut request,
            Some(&filter),
            &[
                "document_id",
                "entity_id",
                "file_id",
                "embedding",
                "embedding_scale",
            ],
        )?;
        let documents = native(index.query(&request), "scan packed sign vectors")?;
        if documents.len() > BINARY_SCAN_PAGE_SIZE {
            if bucket_end.saturating_sub(bucket_start) <= 1 {
                return Err(EngineError::unsupported(
                    "binary vector scan bucket exceeds its bounded page size",
                ));
            }
            let midpoint = bucket_start + (bucket_end - bucket_start) / 2;
            self.scan_binary_sign_range(
                index,
                embedding,
                query,
                limit,
                Some(&filter),
                bucket_start,
                midpoint,
                top_hits,
            )?;
            return self.scan_binary_sign_range(
                index,
                embedding,
                query,
                limit,
                Some(&filter),
                midpoint,
                bucket_end,
                top_hits,
            );
        }

        let mut range_hits = Vec::with_capacity(documents.len());
        for document in documents {
            let packed = native(document.get_binary("embedding"), "read packed sign vector")?
                .ok_or_else(|| corrupt("binary vector document is missing its packed embedding"))?;
            let scale = native(
                document.get_f32("embedding_scale"),
                "read sign-vector scale",
            )?
            .ok_or_else(|| corrupt("binary vector document is missing its embedding scale"))?;
            let score = binary_sign_score(query, &packed, scale, embedding.metric())?;
            range_hits.push(decode_search_hit_with_score(
                &document,
                StorageSearchPath::Vector,
                score,
            )?);
        }
        sort_search_hits(&mut range_hits);
        range_hits.truncate(limit);
        top_hits.extend(range_hits);
        sort_search_hits(top_hits);
        top_hits.truncate(limit);
        Ok(())
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

fn fragments_schema(
    dimension: u32,
    metric: MetricType,
    vector_index_quantization: VectorIndexQuantization,
) -> EngineResult<CollectionSchema> {
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

    match vector_index_quantization {
        VectorIndexQuantization::Fp16 => {
            let mut vector = native(
                FieldSchema::new("embedding", DataType::VectorFp16, false, dimension),
                "define vector field",
            )?;
            native(
                vector.set_index_params(&native(
                    IndexParams::hnsw(metric, 50, 500),
                    "define vector index",
                )?),
                "attach vector index",
            )?;
            native(schema.add_field(&vector), "add vector field")?;
        }
        VectorIndexQuantization::BinarySign => {
            let embedding = native(
                FieldSchema::new("embedding", DataType::Binary, false, 0),
                "define packed sign field",
            )?;
            native(schema.add_field(&embedding), "add packed sign field")?;

            let scale = native(
                FieldSchema::new("embedding_scale", DataType::Float, false, 0),
                "define sign-vector scale field",
            )?;
            native(schema.add_field(&scale), "add sign-vector scale field")?;

            let mut bucket = native(
                FieldSchema::new("vector_scan_bucket", DataType::Uint32, false, 0),
                "define vector scan bucket field",
            )?;
            native(
                bucket.set_index_params(&native(
                    IndexParams::invert(true, false),
                    "define vector scan bucket index",
                )?),
                "attach vector scan bucket index",
            )?;
            native(schema.add_field(&bucket), "add vector scan bucket field")?;
        }
    }
    Ok(schema)
}

fn encode_sign_vector(vector: &[f32]) -> EngineResult<(Vec<u8>, f32)> {
    if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
        return Err(EngineError::invalid_argument(
            "binary-sign document vector must be non-empty and finite",
        ));
    }
    let mut packed = vec![0_u8; vector.len().div_ceil(u8::BITS as usize)];
    let mut absolute_sum = 0.0_f64;
    for (dimension, value) in vector.iter().enumerate() {
        absolute_sum += f64::from(value.abs());
        if *value >= 0.0 {
            packed[dimension / u8::BITS as usize] |= 1_u8 << (dimension % u8::BITS as usize);
        }
    }
    let scale = (absolute_sum / vector.len() as f64) as f32;
    Ok((packed, scale))
}

fn vector_scan_bucket(fragment_id: &str) -> EngineResult<u32> {
    let hash = sha256_hex_parts([b"vector-scan-bucket", fragment_id.as_bytes()]);
    let prefix = hash
        .get(..4)
        .ok_or_else(|| EngineError::internal("binary vector bucket hash is too short"))?;
    u16::from_str_radix(prefix, 16)
        .map(u32::from)
        .map_err(|error| EngineError::internal(format!("decode vector scan bucket: {error}")))
}

fn binary_sign_score(
    query: &[f32],
    packed: &[u8],
    scale: f32,
    metric: EmbeddingMetric,
) -> EngineResult<f64> {
    if !scale.is_finite() || scale < 0.0 {
        return Err(corrupt("binary embedding scale is invalid"));
    }
    let expected_bytes = query.len().div_ceil(u8::BITS as usize);
    if packed.len() != expected_bytes {
        return Err(corrupt(format!(
            "packed sign embedding has {} bytes; expected {expected_bytes}",
            packed.len()
        )));
    }
    let mut dot = 0.0_f64;
    let mut query_norm_squared = 0.0_f64;
    for (dimension, value) in query.iter().enumerate() {
        if !value.is_finite() {
            return Err(EngineError::invalid_argument(
                "binary-sign query contains a non-finite value",
            ));
        }
        let value = f64::from(*value);
        query_norm_squared += value * value;
        let positive =
            packed[dimension / u8::BITS as usize] & (1_u8 << (dimension % u8::BITS as usize)) != 0;
        dot += value * if positive { 1.0 } else { -1.0 };
    }
    match metric {
        EmbeddingMetric::Cosine => {
            if query_norm_squared == 0.0 || scale == 0.0 {
                Ok(0.0)
            } else {
                Ok(dot / (query_norm_squared.sqrt() * (query.len() as f64).sqrt()))
            }
        }
        EmbeddingMetric::DotProduct => Ok(dot * f64::from(scale)),
        EmbeddingMetric::Euclidean => {
            let scale = f64::from(scale);
            let distance_squared = (query_norm_squared + query.len() as f64 * scale * scale
                - 2.0 * scale * dot)
                .max(0.0);
            Ok(-distance_squared.sqrt())
        }
    }
}

fn sort_search_hits(hits: &mut [StorageSearchHit]) {
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.document_id.cmp(&right.document_id))
    });
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
    decode_search_hit_with_score(doc, path, f64::from(doc.get_score()))
}

fn decode_search_hit_with_score(
    doc: &Doc,
    path: StorageSearchPath,
    score: f64,
) -> EngineResult<StorageSearchHit> {
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
        score,
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
