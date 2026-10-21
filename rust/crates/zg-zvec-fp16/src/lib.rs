//! Store and query zvec embedding fields as IEEE binary16.
//!
//! Embeddings stay `f32` in the engine. They are narrowed to FP16 when written
//! and when a vector query is built. Reads widen the stored bits back to `f32`.
//! zvec-rust 0.7.2 can declare a `VectorFp16` field, but its safe document and
//! query APIs only accept FP32 bytes. Those bytes fail zvec's dimension check
//! on an FP16 field, so this crate passes binary16 bytes through the C API.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

use half::f16;
use zvec_rust::sys::{self, zvec_error_code_t};
use zvec_rust::{Doc, Error, ErrorCode, Result, SearchQuery};

/// Writes an embedding as IEEE binary16 values.
///
/// # Errors
///
/// Returns an error when `name` contains a null byte or zvec rejects the field.
pub fn add_embedding(doc: &mut Doc, name: &str, vector: &[f32]) -> Result<()> {
    let encoded = encode(vector);
    let c_name = to_cstring(name)?;
    check_error(unsafe {
        // SAFETY: `doc` owns a live zvec document, `c_name` is a valid C string,
        // and `encoded` stays alive for this call. zvec copies the bytes.
        sys::zvec_doc_add_field_by_value(
            doc.as_raw(),
            c_name.as_ptr(),
            sys::ZVEC_DATA_TYPE_VECTOR_FP16,
            encoded.as_ptr().cast::<c_void>(),
            std::mem::size_of_val(encoded.as_slice()),
        )
    })
}

/// Builds a vector query whose payload matches an FP16 field.
///
/// The caller passes the original `f32` query. It is converted to binary16
/// before zvec sees it, which is the width zvec requires for `VectorFp16`.
///
/// # Errors
///
/// Returns an error when `field` contains a null byte, the query vector is
/// empty, or zvec rejects the query.
pub fn embedding_query(field: &str, vector: &[f32], topk: i32) -> Result<SearchQuery> {
    if vector.is_empty() {
        return Err(invalid_argument("embedding query vector is empty"));
    }
    let encoded = encode(vector);
    let handle = unsafe { sys::zvec_vector_query_create() };
    if handle.is_null() {
        return Err(Error {
            code: ErrorCode::InternalError,
            message: "failed to create vector query".into(),
        });
    }
    // SAFETY: the handle was just created and is non-null. `SearchQuery` owns it.
    let query = unsafe { SearchQuery::from_raw(handle) };
    let c_field = to_cstring(field)?;
    check_error(unsafe {
        // SAFETY: `query` owns the handle and `c_field` outlives the call.
        sys::zvec_vector_query_set_field_name(query.as_raw(), c_field.as_ptr())
    })?;
    check_error(unsafe {
        // SAFETY: `encoded` stays alive for this call. zvec copies the bytes.
        sys::zvec_vector_query_set_query_vector(
            query.as_raw(),
            encoded.as_ptr().cast::<c_void>(),
            std::mem::size_of_val(encoded.as_slice()),
        )
    })?;
    check_error(unsafe {
        // SAFETY: `query` still owns the handle.
        sys::zvec_vector_query_set_topk(query.as_raw(), topk)
    })?;
    Ok(query)
}

/// Reads an FP16 embedding and widens each element to `f32`.
///
/// Returns `Ok(None)` when the field is absent or null.
///
/// # Errors
///
/// Returns an error when `name` contains a null byte, the stored byte length
/// is not a multiple of two, or zvec cannot read the field.
pub fn decode_embedding(doc: &Doc, name: &str) -> Result<Option<Vec<f32>>> {
    if !doc.has_field(name) || doc.is_field_null(name) {
        return Ok(None);
    }
    let c_name = to_cstring(name)?;
    let mut value_ptr: *const c_void = std::ptr::null();
    let mut value_size = 0usize;
    check_error(unsafe {
        // SAFETY: `doc` owns a live document and `c_name` outlives the call.
        // The returned pointer is borrowed from the document.
        sys::zvec_doc_get_field_value_pointer(
            doc.as_raw(),
            c_name.as_ptr(),
            sys::ZVEC_DATA_TYPE_VECTOR_FP16,
            &raw mut value_ptr,
            &raw mut value_size,
        )
    })?;
    if value_ptr.is_null() || value_size == 0 {
        return Ok(None);
    }
    if !value_size.is_multiple_of(2) {
        return Err(Error {
            code: ErrorCode::InternalError,
            message: format!("fp16 embedding byte length {value_size} is not even"),
        });
    }
    let mut bytes = vec![0u8; value_size];
    unsafe {
        // SAFETY: zvec reported `value_size` bytes at `value_ptr` for this field.
        std::ptr::copy_nonoverlapping(value_ptr.cast::<u8>(), bytes.as_mut_ptr(), value_size);
    }
    let (chunks, _) = bytes.as_chunks::<2>();
    Ok(Some(
        chunks
            .iter()
            .map(|chunk| f16::from_bits(u16::from_ne_bytes(*chunk)).to_f32())
            .collect(),
    ))
}

fn encode(vector: &[f32]) -> Vec<f16> {
    vector.iter().copied().map(f16::from_f32).collect()
}

fn invalid_argument(message: &str) -> Error {
    Error {
        code: ErrorCode::InvalidArgument,
        message: message.to_owned(),
    }
}

fn to_cstring(value: &str) -> Result<CString> {
    CString::new(value).map_err(|_| invalid_argument("string contains null byte"))
}

fn check_error(code: zvec_error_code_t) -> Result<()> {
    if code == sys::ZVEC_OK {
        return Ok(());
    }
    let message = unsafe {
        let mut c_msg: *mut c_char = std::ptr::null_mut();
        // SAFETY: zvec writes an allocated message or leaves the pointer null.
        sys::zvec_get_last_error(&raw mut c_msg);
        if c_msg.is_null() {
            "unknown error".to_owned()
        } else {
            // SAFETY: `c_msg` is a zvec-allocated C string that must be freed.
            let text = CStr::from_ptr(c_msg).to_string_lossy().into_owned();
            sys::zvec_free(c_msg.cast::<c_void>());
            text
        }
    };
    Err(Error {
        code: ErrorCode::from(code),
        message,
    })
}

#[cfg(test)]
mod tests {
    use zvec_rust::{Collection, CollectionSchema, DataType, Doc, IndexParams, MetricType};

    use super::{add_embedding, decode_embedding, embedding_query, f16};

    fn initialize() {
        zvec_rust::initialize(None).expect("initialize zvec");
    }

    #[test]
    fn f16_values_round_trip_to_the_same_binary16_bits() {
        let samples = [0.0, -0.0, 1.0, -1.5, 0.1, 1.25, -0.2, 65504.0];
        for value in samples {
            let encoded = f16::from_f32(value);
            assert_eq!(
                encoded.to_bits(),
                f16::from_bits(encoded.to_bits()).to_bits()
            );
            assert_eq!(f16::from_f32(encoded.to_f32()).to_bits(), encoded.to_bits());
        }
        assert_ne!(f16::from_f32(0.1).to_f32().to_bits(), 0.1_f32.to_bits());
    }

    #[test]
    fn stored_fp16_embeddings_round_trip_and_search() {
        initialize();
        let directory = tempfile::tempdir().expect("collection directory");
        let path = directory.path().join("embeddings");
        let schema = CollectionSchema::builder("embeddings")
            .add_vector_field(
                "embedding",
                DataType::VectorFp16,
                4,
                IndexParams::flat(MetricType::Cosine).expect("flat index"),
            )
            .build()
            .expect("schema");
        let collection =
            Collection::create_and_open(path.to_str().expect("utf-8 path"), &schema, None)
                .expect("create collection");

        let original = [0.1_f32, -0.2, 1.25, 0.0];
        let neighbor = [0.1_f32, -0.2, 1.0, 0.0];
        let distant = [0.0_f32, 1.0, 0.0, 0.0];
        for (key, vector) in [("near", original), ("mid", neighbor), ("far", distant)] {
            let mut doc = Doc::new().expect("document");
            doc.set_pk(key);
            add_embedding(&mut doc, "embedding", vector.as_slice()).expect("encode embedding");
            collection.insert(&[&doc]).expect("insert");
        }

        let stored = collection.fetch(&["near"]).expect("fetch");
        assert_eq!(stored.len(), 1);
        let decoded = decode_embedding(&stored[0], "embedding")
            .expect("decode")
            .expect("stored vector");
        assert_eq!(decoded.len(), original.len());
        for (decoded, original) in decoded.iter().zip(original) {
            let rounded = f16::from_f32(original).to_f32();
            assert_eq!(decoded.to_bits(), rounded.to_bits());
        }
        assert_ne!(
            decoded[0].to_bits(),
            original[0].to_bits(),
            "0.1 is not exact in binary16"
        );

        let query = embedding_query("embedding", &original, 3).expect("query");
        let hits = collection.query(&query).expect("search");
        let keys = hits
            .iter()
            .map(|doc| doc.get_pk().expect("primary key").to_owned())
            .collect::<Vec<_>>();
        assert_eq!(keys.first().map(String::as_str), Some("near"));
        assert!(keys.contains(&"mid".to_owned()));
    }
}
