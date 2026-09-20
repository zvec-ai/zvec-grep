use std::ffi::CStr;
use std::marker::PhantomData;
use std::ptr;

use crate::doc::Doc;
use crate::error::{check_error, to_cstring, Error, ErrorCode, Result};
use crate::multi_query::MultiQuery;
use crate::query::SearchQuery;
use crate::schema::{CollectionSchema, FieldSchema, IndexParams};

/// Options for creating or opening a collection.
pub struct CollectionOptions {
    pub(crate) handle: *mut zvec_rust_sys::zvec_collection_options_t,
}

impl CollectionOptions {
    /// Creates new collection options with default values.
    pub fn new() -> Result<Self> {
        let handle = unsafe { zvec_rust_sys::zvec_collection_options_create() };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create collection options".into(),
            });
        }
        Ok(CollectionOptions { handle })
    }

    /// Sets whether to enable memory mapping.
    pub fn set_enable_mmap(&mut self, enable: bool) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_options_set_enable_mmap(self.handle, enable)
        })
    }

    /// Returns whether memory mapping is enabled.
    pub fn enable_mmap(&self) -> bool {
        unsafe { zvec_rust_sys::zvec_collection_options_get_enable_mmap(self.handle) }
    }

    /// Sets the maximum buffer size in bytes.
    pub fn set_max_buffer_size(&mut self, size: u64) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_options_set_max_buffer_size(self.handle, size as usize)
        })
    }

    /// Returns the maximum buffer size in bytes.
    pub fn max_buffer_size(&self) -> u64 {
        unsafe { zvec_rust_sys::zvec_collection_options_get_max_buffer_size(self.handle) as u64 }
    }

    /// Sets whether the collection is read-only.
    pub fn set_read_only(&mut self, read_only: bool) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_options_set_read_only(self.handle, read_only)
        })
    }

    /// Returns whether the collection is read-only.
    pub fn read_only(&self) -> bool {
        unsafe { zvec_rust_sys::zvec_collection_options_get_read_only(self.handle) }
    }
}

impl Drop for CollectionOptions {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_collection_options_destroy(self.handle) };
        }
    }
}

/// Statistics about a single index in a collection.
#[derive(Debug, Clone)]
pub struct IndexStat {
    pub name: String,
    pub completeness: f32,
}

/// Statistics about a collection.
#[derive(Debug, Clone)]
pub struct CollectionStats {
    pub doc_count: u64,
    pub indexes: Vec<IndexStat>,
}

/// Per-document result of a write operation.
#[derive(Debug, Clone)]
pub struct DocWriteResult {
    pub success: bool,
    pub code: ErrorCode,
    pub message: String,
}

impl DocWriteResult {
    /// Returns `true` if this individual write succeeded.
    pub fn is_success(&self) -> bool {
        self.success
    }
}

/// Result of a write operation (insert/update/upsert/delete).
#[derive(Debug, Clone)]
pub struct WriteResult {
    pub success_count: u64,
    pub error_count: u64,
    pub results: Vec<DocWriteResult>,
}

/// A zvec collection for storing and querying vector data.
///
/// Collections are the primary data container in zvec. They hold documents
/// with typed fields and support vector similarity search.
///
/// The collection is automatically closed when dropped.
pub struct Collection {
    handle: *mut zvec_rust_sys::zvec_collection_t,
}

impl Collection {
    /// Returns the raw FFI handle.
    ///
    /// # Safety
    /// The caller must not use the handle after the `Collection` is dropped.
    pub unsafe fn as_raw(&self) -> *mut zvec_rust_sys::zvec_collection_t {
        self.handle
    }

    /// Creates a `Collection` from a raw FFI handle.
    ///
    /// # Safety
    /// The caller must ensure the handle is valid and was created by the zvec C API.
    /// The `Collection` takes ownership and will call `zvec_collection_close` on drop.
    pub unsafe fn from_raw(handle: *mut zvec_rust_sys::zvec_collection_t) -> Self {
        Collection { handle }
    }

    /// Creates a new collection and opens it.
    pub fn create_and_open(
        path: &str,
        schema: &CollectionSchema,
        options: Option<&CollectionOptions>,
    ) -> Result<Self> {
        let c_path = to_cstring(path)?;
        let c_options = options.map(|o| o.handle as *const _).unwrap_or(ptr::null());

        let mut handle: *mut zvec_rust_sys::zvec_collection_t = ptr::null_mut();
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_create_and_open(
                c_path.as_ptr(),
                schema.handle,
                c_options,
                &mut handle,
            )
        })?;

        Ok(Collection { handle })
    }

    /// Opens an existing collection.
    pub fn open(path: &str, options: Option<&CollectionOptions>) -> Result<Self> {
        let c_path = to_cstring(path)?;
        let c_options = options.map(|o| o.handle as *const _).unwrap_or(ptr::null());

        let mut handle: *mut zvec_rust_sys::zvec_collection_t = ptr::null_mut();
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_open(c_path.as_ptr(), c_options, &mut handle)
        })?;

        Ok(Collection { handle })
    }

    /// Flushes collection data to disk.
    pub fn flush(&self) -> Result<()> {
        check_error(unsafe { zvec_rust_sys::zvec_collection_flush(self.handle) })
    }

    /// Returns the collection schema.
    pub fn schema(&self) -> Result<CollectionSchema> {
        let mut schema_handle: *mut zvec_rust_sys::zvec_collection_schema_t = ptr::null_mut();
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_get_schema(self.handle, &mut schema_handle)
        })?;
        Ok(CollectionSchema::from_owned(schema_handle))
    }

    /// Returns collection statistics.
    pub fn stats(&self) -> Result<CollectionStats> {
        let mut stats_handle: *mut zvec_rust_sys::zvec_collection_stats_t = ptr::null_mut();
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_get_stats(self.handle, &mut stats_handle)
        })?;

        let doc_count = unsafe { zvec_rust_sys::zvec_collection_stats_get_doc_count(stats_handle) };
        let index_count =
            unsafe { zvec_rust_sys::zvec_collection_stats_get_index_count(stats_handle) };

        let mut indexes = Vec::with_capacity(index_count);

        for i in 0..index_count {
            let name_ptr =
                unsafe { zvec_rust_sys::zvec_collection_stats_get_index_name(stats_handle, i) };
            let name = if name_ptr.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(name_ptr).to_string_lossy().into_owned() }
            };

            let completeness = unsafe {
                zvec_rust_sys::zvec_collection_stats_get_index_completeness(stats_handle, i)
            };
            indexes.push(IndexStat { name, completeness });
        }

        unsafe { zvec_rust_sys::zvec_collection_stats_destroy(stats_handle) };

        Ok(CollectionStats { doc_count, indexes })
    }

    // =========================================================================
    // DML Operations
    // =========================================================================

    /// Inserts documents into the collection.
    pub fn insert(&self, docs: &[&Doc]) -> Result<WriteResult> {
        let ptrs: Vec<*const zvec_rust_sys::zvec_doc_t> =
            docs.iter().map(|d| d.handle as *const _).collect();
        let mut results: *mut zvec_rust_sys::zvec_write_result_t = ptr::null_mut();
        let mut result_count: usize = 0;

        check_error(unsafe {
            zvec_rust_sys::zvec_collection_insert_with_results(
                self.handle,
                ptrs.as_ptr(),
                ptrs.len(),
                &mut results,
                &mut result_count,
            )
        })?;

        Ok(collect_write_results(results, result_count))
    }

    /// Updates documents in the collection.
    pub fn update(&self, docs: &[&Doc]) -> Result<WriteResult> {
        let ptrs: Vec<*const zvec_rust_sys::zvec_doc_t> =
            docs.iter().map(|d| d.handle as *const _).collect();
        let mut results: *mut zvec_rust_sys::zvec_write_result_t = ptr::null_mut();
        let mut result_count: usize = 0;

        check_error(unsafe {
            zvec_rust_sys::zvec_collection_update_with_results(
                self.handle,
                ptrs.as_ptr(),
                ptrs.len(),
                &mut results,
                &mut result_count,
            )
        })?;

        Ok(collect_write_results(results, result_count))
    }

    /// Inserts or updates documents (upsert).
    pub fn upsert(&self, docs: &[&Doc]) -> Result<WriteResult> {
        let ptrs: Vec<*const zvec_rust_sys::zvec_doc_t> =
            docs.iter().map(|d| d.handle as *const _).collect();
        let mut results: *mut zvec_rust_sys::zvec_write_result_t = ptr::null_mut();
        let mut result_count: usize = 0;

        check_error(unsafe {
            zvec_rust_sys::zvec_collection_upsert_with_results(
                self.handle,
                ptrs.as_ptr(),
                ptrs.len(),
                &mut results,
                &mut result_count,
            )
        })?;

        Ok(collect_write_results(results, result_count))
    }

    /// Deletes documents by primary keys.
    pub fn delete(&self, pks: &[&str]) -> Result<WriteResult> {
        let c_pks: Vec<_> = pks
            .iter()
            .map(|pk| to_cstring(pk))
            .collect::<Result<Vec<_>>>()?;
        let c_ptrs: Vec<_> = c_pks.iter().map(|pk| pk.as_ptr()).collect();
        let mut results: *mut zvec_rust_sys::zvec_write_result_t = ptr::null_mut();
        let mut result_count: usize = 0;

        check_error(unsafe {
            zvec_rust_sys::zvec_collection_delete_with_results(
                self.handle,
                c_ptrs.as_ptr(),
                c_ptrs.len(),
                &mut results,
                &mut result_count,
            )
        })?;

        Ok(collect_write_results(results, result_count))
    }

    /// Deletes documents matching a filter expression.
    pub fn delete_by_filter(&self, filter: &str) -> Result<()> {
        let c_filter = to_cstring(filter)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_delete_by_filter(self.handle, c_filter.as_ptr())
        })
    }

    // =========================================================================
    // DQL Operations
    // =========================================================================

    /// Performs a vector similarity search.
    pub fn query(&self, query: &SearchQuery) -> Result<Vec<Doc>> {
        let mut results: *mut *mut zvec_rust_sys::zvec_doc_t = ptr::null_mut();
        let mut result_count: usize = 0;

        check_error(unsafe {
            zvec_rust_sys::zvec_collection_query(
                self.handle,
                query.handle,
                &mut results,
                &mut result_count,
            )
        })?;

        let docs = unsafe { collect_docs(results, result_count) };
        Ok(docs)
    }

    /// Performs a multi-query that combines several sub-queries with a rerank
    /// strategy (RRF or weighted).
    pub fn multi_query(&self, query: &MultiQuery) -> Result<Vec<Doc>> {
        let mut results: *mut *mut zvec_rust_sys::zvec_doc_t = ptr::null_mut();
        let mut result_count: usize = 0;

        check_error(unsafe {
            zvec_rust_sys::zvec_collection_multi_query(
                self.handle,
                query.handle,
                &mut results,
                &mut result_count,
            )
        })?;

        let docs = unsafe { collect_docs(results, result_count) };
        Ok(docs)
    }

    /// Fetches documents by primary keys, returning all fields including vectors.
    pub fn fetch(&self, pks: &[&str]) -> Result<Vec<Doc>> {
        self.fetch_with_options(pks, None, true)
    }

    /// Fetches documents by primary keys with control over which fields to return.
    pub fn fetch_with_options(
        &self,
        pks: &[&str],
        output_fields: Option<&[&str]>,
        include_vector: bool,
    ) -> Result<Vec<Doc>> {
        let c_pks: Vec<_> = pks
            .iter()
            .map(|pk| to_cstring(pk))
            .collect::<Result<Vec<_>>>()?;
        let c_pk_ptrs: Vec<_> = c_pks.iter().map(|pk| pk.as_ptr()).collect();

        let c_fields = output_fields
            .map(|fields| {
                fields
                    .iter()
                    .map(|f| to_cstring(f))
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        let c_field_ptrs: Option<Vec<_>> = c_fields
            .as_ref()
            .map(|f| f.iter().map(|s| s.as_ptr()).collect());
        let (fields_ptr, fields_count) = match &c_field_ptrs {
            Some(ptrs) => (ptrs.as_ptr(), ptrs.len()),
            None => (ptr::null(), 0),
        };

        let mut documents: *mut *mut zvec_rust_sys::zvec_doc_t = ptr::null_mut();
        let mut found_count: usize = 0;

        check_error(unsafe {
            zvec_rust_sys::zvec_collection_fetch(
                self.handle,
                c_pk_ptrs.as_ptr(),
                c_pk_ptrs.len(),
                fields_ptr,
                fields_count,
                include_vector,
                &mut documents,
                &mut found_count,
            )
        })?;

        let docs = unsafe { collect_docs(documents, found_count) };
        Ok(docs)
    }

    // =========================================================================
    // Document Iteration
    // =========================================================================

    /// Creates an iterator over all documents in the collection using default
    /// options (all scalar fields, vectors included).
    ///
    /// The iterator traverses an isolated snapshot taken at creation time;
    /// documents written afterwards are not visible.
    pub fn iter(&self) -> Result<DocIterator<'_>> {
        self.iter_with_options(None, true)
    }

    /// Creates an iterator over all documents with control over which fields
    /// to return.
    ///
    /// - `output_fields`: scalar fields to return; `None` returns all fields.
    ///   An empty slice returns no scalar fields (primary key only).
    /// - `include_vector`: whether to include vector fields.
    ///
    /// The iterator traverses an isolated snapshot taken at creation time;
    /// documents written afterwards are not visible. While the iterator is
    /// open, schema changes (create/drop index, add/alter/drop column) and
    /// destroy are rejected by the C library.
    pub fn iter_with_options(
        &self,
        output_fields: Option<&[&str]>,
        include_vector: bool,
    ) -> Result<DocIterator<'_>> {
        let options = unsafe { zvec_rust_sys::zvec_iterator_options_create() };
        if options.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create iterator options".into(),
            });
        }

        // Build C string fields before the first FFI call that must be cleaned
        // up on failure; `options` is destroyed on every early return.
        let result = (|| -> Result<()> {
            let c_fields = output_fields
                .map(|fields| {
                    fields
                        .iter()
                        .map(|f| to_cstring(f))
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?;
            let c_field_ptrs: Option<Vec<_>> = c_fields
                .as_ref()
                .map(|f| f.iter().map(|s| s.as_ptr()).collect());
            let (fields_ptr, fields_count) = match &c_field_ptrs {
                Some(ptrs) => (ptrs.as_ptr(), ptrs.len()),
                None => (ptr::null(), 0),
            };

            check_error(unsafe {
                zvec_rust_sys::zvec_iterator_options_set_output_fields(
                    options,
                    fields_ptr,
                    fields_count,
                )
            })?;
            check_error(unsafe {
                zvec_rust_sys::zvec_iterator_options_set_include_vector(options, include_vector)
            })?;
            Ok(())
        })();
        if let Err(e) = result {
            unsafe { zvec_rust_sys::zvec_iterator_options_destroy(options) };
            return Err(e);
        }

        let mut iter: *mut zvec_rust_sys::zvec_doc_iterator_t = ptr::null_mut();
        let rc = check_error(unsafe {
            zvec_rust_sys::zvec_collection_create_iterator(self.handle, options, &mut iter)
        });
        unsafe { zvec_rust_sys::zvec_iterator_options_destroy(options) };
        rc?;

        Ok(DocIterator {
            handle: iter,
            _collection: PhantomData,
        })
    }

    // =========================================================================
    // Index Management
    // =========================================================================

    /// Creates an index on a field.
    pub fn create_index(&self, field_name: &str, params: &IndexParams) -> Result<()> {
        let c_name = to_cstring(field_name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_create_index(self.handle, c_name.as_ptr(), params.handle)
        })
    }

    /// Drops an index from a field.
    pub fn drop_index(&self, field_name: &str) -> Result<()> {
        let c_name = to_cstring(field_name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_drop_index(self.handle, c_name.as_ptr())
        })
    }

    /// Optimizes the collection (rebuild indexes, merge segments, etc.).
    pub fn optimize(&self) -> Result<()> {
        check_error(unsafe { zvec_rust_sys::zvec_collection_optimize(self.handle) })
    }

    // =========================================================================
    // DDL Operations
    // =========================================================================

    /// Adds a column to the collection.
    pub fn add_column(&self, field_schema: &FieldSchema, default_expr: Option<&str>) -> Result<()> {
        let c_expr_owned = default_expr.map(to_cstring).transpose()?;
        let c_expr_ptr = c_expr_owned
            .as_ref()
            .map(|s| s.as_ptr())
            .unwrap_or(ptr::null());
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_add_column(self.handle, field_schema.handle, c_expr_ptr)
        })
    }

    /// Drops a column from the collection.
    pub fn drop_column(&self, name: &str) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_drop_column(self.handle, c_name.as_ptr())
        })
    }

    /// Closes the collection explicitly.
    pub fn close(self) -> Result<()> {
        // Drop will handle the close
        drop(self);
        Ok(())
    }
}

impl Drop for Collection {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // Safety: handle was created by zvec_collection_create_and_open or zvec_collection_open
            let rc = unsafe { zvec_rust_sys::zvec_collection_close(self.handle) };
            if rc != zvec_rust_sys::ZVEC_OK {
                eprintln!(
                    "zvec warning: failed to close collection (error code {})",
                    rc
                );
            }
        }
    }
}

// Safety: The zvec C-API documents that collection operations are internally
// thread-safe. All mutable state is protected by internal locks in the C library.
// See: https://github.com/alibaba/zvec — C-API thread-safety guarantees.
unsafe impl Send for Collection {}
unsafe impl Sync for Collection {}

/// Iterator over all documents in a collection.
///
/// Created by [`Collection::iter`] or [`Collection::iter_with_options`].
/// Traverses an isolated snapshot taken at creation time. Each `next` call
/// returns `Some(Ok(doc))` for a document, `Some(Err(e))` on failure, and
/// `None` at the end of the collection.
///
/// The iterator borrows its collection, so the collection cannot be dropped
/// while the iterator is alive. Dropping the iterator closes it via
/// `zvec_doc_iterator_close`.
pub struct DocIterator<'a> {
    handle: *mut zvec_rust_sys::zvec_doc_iterator_t,
    _collection: PhantomData<&'a Collection>,
}

impl<'a> Iterator for DocIterator<'a> {
    type Item = Result<Doc>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut doc: *mut zvec_rust_sys::zvec_doc_t = ptr::null_mut();
        let rc = unsafe { zvec_rust_sys::zvec_doc_iterator_next(self.handle, &mut doc) };
        if rc != zvec_rust_sys::ZVEC_OK {
            return Some(Err(Error {
                code: ErrorCode::from(rc),
                message: format!("document iterator failed with error code {}", rc),
            }));
        }
        if doc.is_null() {
            return None; // EOF
        }
        // Take ownership: Rust will call zvec_doc_destroy on drop.
        Some(Ok(unsafe { Doc::from_raw(doc) }))
    }
}

impl<'a> Drop for DocIterator<'a> {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_doc_iterator_close(self.handle) };
        }
    }
}

/// Parses a C array of `zvec_write_result_t` into a `WriteResult`.
fn collect_write_results(
    results: *mut zvec_rust_sys::zvec_write_result_t,
    count: usize,
) -> WriteResult {
    let mut doc_results = Vec::with_capacity(count);
    let mut success_count: u64 = 0;
    let mut error_count: u64 = 0;

    if !results.is_null() && count > 0 {
        for i in 0..count {
            let wr = unsafe { &*results.add(i) };
            let is_ok = wr.code == zvec_rust_sys::ZVEC_OK;
            let message = if wr.message.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(wr.message).to_string_lossy().into_owned() }
            };
            if is_ok {
                success_count += 1;
            } else {
                error_count += 1;
            }
            doc_results.push(DocWriteResult {
                success: is_ok,
                code: ErrorCode::from(wr.code),
                message,
            });
        }
        unsafe { zvec_rust_sys::zvec_write_results_free(results, count) };
    }

    WriteResult {
        success_count,
        error_count,
        results: doc_results,
    }
}

/// Collects document pointers from a C array into a Vec<Doc>.
///
/// # Safety
/// `results` must point to a valid array of `count` document pointers
/// allocated by the zvec C library.
unsafe fn collect_docs(results: *mut *mut zvec_rust_sys::zvec_doc_t, count: usize) -> Vec<Doc> {
    if results.is_null() || count == 0 {
        return Vec::new();
    }

    let mut docs = Vec::with_capacity(count);
    for i in 0..count {
        let doc_ptr = *results.add(i);
        if !doc_ptr.is_null() {
            // Take ownership: Rust will call zvec_doc_destroy on drop
            docs.push(Doc::from_raw(doc_ptr));
        }
    }

    // Free only the pointer array itself (not the individual docs, which are now
    // owned by the Doc wrappers above and will be freed via their Drop impls).
    zvec_rust_sys::zvec_free(results as *mut std::os::raw::c_void);

    docs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_docs_handles_null_pointer() {
        let docs = unsafe { collect_docs(ptr::null_mut(), 0) };
        assert!(docs.is_empty());
    }

    #[test]
    fn collect_docs_handles_zero_count() {
        // Even with a non-null pointer, zero count should return empty
        let mut fake: *mut zvec_rust_sys::zvec_doc_t = ptr::null_mut();
        let docs = unsafe { collect_docs(&mut fake as *mut _, 0) };
        assert!(docs.is_empty());
    }
}
