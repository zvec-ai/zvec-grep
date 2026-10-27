//! Shared zvec initialization and collection operations.
use std::{
    collections::{BTreeSet, HashMap},
    fmt::Write,
    path::Path,
    sync::OnceLock,
};
use zvec_rust::{
    Collection, CollectionOptions, CollectionSchema, DataType, Doc, FieldSchema, IndexParams,
    SearchQuery,
};

use crate::{EngineError, EngineResult};

pub(super) const WRITE_BATCH: usize = 1024;
static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();

pub(super) fn initialize() -> EngineResult<()> {
    match INITIALIZED.get_or_init(|| {
        let config = zvec_rust::ConfigBuilder::new()
            .num_threads(2)
            .memory_limit(512 * 1024 * 1024)
            .build();
        zvec_rust::initialize(Some(&config)).map_err(|error| error.to_string())
    }) {
        Ok(()) => Ok(()),
        Err(message) => Err(EngineError::storage_failure(format!(
            "cannot initialize zvec: {message}"
        ))),
    }
}

pub(super) fn open_collection(
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

pub(super) fn scalar(
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

pub(super) fn wildcard_string(
    schema: &mut CollectionSchema,
    name: &str,
    nullable: bool,
) -> EngineResult<()> {
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

pub(super) fn fetch_map(
    collection: &Collection,
    keys: &[String],
) -> EngineResult<HashMap<String, Doc>> {
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

pub(super) fn write_docs(
    collection: &Collection,
    docs: &[Doc],
    operation: &str,
) -> EngineResult<()> {
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

pub(super) fn u32_field(doc: &Doc, name: &str) -> EngineResult<u32> {
    native(doc.get_u32(name), "read numeric field")?.ok_or_else(|| corrupt("missing numeric field"))
}

pub(super) fn doc_key(doc: &Doc) -> EngineResult<&str> {
    doc.get_pk()
        .ok_or_else(|| corrupt("stored record has no primary key"))
}

pub(super) fn string_field(doc: &Doc, field: &str) -> EngineResult<String> {
    native(doc.get_string(field), "read stored field")?
        .ok_or_else(|| corrupt(format!("stored field {field} is missing")))
}

pub(super) fn corrupt(message: impl std::fmt::Display) -> EngineError {
    EngineError::storage_failure(message.to_string())
}
#[track_caller]
pub(super) fn native<T>(result: zvec_rust::Result<T>, operation: &str) -> EngineResult<T> {
    result.map_err(|error| EngineError::storage_failure(format!("zvec {operation}: {error}")))
}

/// Read every distinct entity ID matching a scalar filter without a result cap.
/// Full batches split into open ranges around an already-collected pivot. This
/// also progresses when one entity has enough fragment rows to fill a batch.
pub(super) fn collect_entity_ids(
    collection: &Collection,
    filter: &str,
    validate: impl Fn(&Doc) -> EngineResult<()>,
) -> EngineResult<Vec<String>> {
    const BATCH_SIZE: usize = 1024;
    let mut ids = BTreeSet::new();
    let mut ranges: Vec<(Option<String>, Option<String>)> = vec![(None, None)];
    while let Some((lower, upper)) = ranges.pop() {
        let mut filter = filter.to_owned();
        if let Some(lower) = &lower {
            let _ = write!(filter, " AND entity_id > {}", quote(lower));
        }
        if let Some(upper) = &upper {
            let _ = write!(filter, " AND entity_id < {}", quote(upper));
        }
        let mut query = native(
            SearchQuery::scalar(i32::try_from(BATCH_SIZE).expect("batch size fits i32")),
            "create entity ID query",
        )?;
        native(query.set_filter(&filter), "filter entity IDs")?;
        native(
            query.set_output_fields(&["file_id", "entity_id"]),
            "project entity IDs",
        )?;
        native(query.set_include_vector(false), "omit entity vectors")?;
        let docs = native(collection.query(&query), "query entity IDs")?;
        let full = docs.len() == BATCH_SIZE;
        let mut batch = BTreeSet::new();
        for doc in docs {
            validate(&doc)?;
            let id = string_field(&doc, "entity_id")?;
            if lower.as_ref().is_some_and(|bound| id <= *bound)
                || upper.as_ref().is_some_and(|bound| id >= *bound)
            {
                return Err(corrupt(
                    "entity query returned an ID outside its requested range",
                ));
            }
            batch.insert(id);
        }
        if full {
            let pivot = batch
                .iter()
                .nth(batch.len() / 2)
                .expect("full batch")
                .clone();
            ranges.push((Some(pivot.clone()), upper));
            ranges.push((lower, Some(pivot)));
        }
        ids.extend(batch);
    }
    Ok(ids.into_iter().collect())
}

pub(super) fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "\\'"))
}
