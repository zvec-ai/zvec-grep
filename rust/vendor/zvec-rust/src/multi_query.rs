//! Multi-query and sub-query types for combined vector searches.
//!
//! A [`MultiQuery`] aggregates multiple [`SubQuery`] instances and reranks the
//! results using either RRF (Reciprocal Rank Fusion) or weighted strategies.

use std::os::raw::c_void;

use crate::error::{check_error, to_cstring, Error, ErrorCode, Result};
use crate::query::Fts;
use crate::query::{
    DiskannQueryParams, FlatQueryParams, FtsQueryParams, HnswQueryParams, IvfQueryParams,
    IvfRabitqQueryParams,
};

/// A multi-query operation combining multiple [`SubQuery`] objects.
///
/// Use [`MultiQuery::new`] to construct, then [`MultiQuery::add_sub_query`] to
/// attach individual sub-queries. Configure top-k, filters, and rerank strategy
/// before passing to [`crate::Collection::multi_query`].
pub struct MultiQuery {
    pub(crate) handle: *mut zvec_rust_sys::zvec_multi_query_t,
}

impl MultiQuery {
    /// Creates a new multi-query.
    pub fn new() -> Result<Self> {
        let handle = unsafe { zvec_rust_sys::zvec_multi_query_create() };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create multi-query".into(),
            });
        }
        Ok(MultiQuery { handle })
    }

    /// Returns the raw FFI handle.
    ///
    /// # Safety
    /// The caller must not use the handle after the `MultiQuery` is dropped.
    pub unsafe fn as_raw(&self) -> *mut zvec_rust_sys::zvec_multi_query_t {
        self.handle
    }

    /// Adds a sub-query to this multi-query.
    ///
    /// The sub-query is copied internally; the caller retains ownership.
    pub fn add_sub_query(&mut self, sub: &SubQuery) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_multi_query_add_sub_query(self.handle, sub.handle)
        })
    }

    /// Returns the number of sub-queries currently registered.
    pub fn sub_query_count(&self) -> usize {
        unsafe { zvec_rust_sys::zvec_multi_query_get_sub_query_count(self.handle) }
    }

    /// Sets the top-k parameter for the merged result.
    pub fn set_topk(&mut self, topk: i32) -> Result<()> {
        check_error(unsafe { zvec_rust_sys::zvec_multi_query_set_topk(self.handle, topk) })
    }

    /// Returns the configured top-k value.
    pub fn topk(&self) -> i32 {
        unsafe { zvec_rust_sys::zvec_multi_query_get_topk(self.handle) }
    }

    /// Sets the filter expression applied to the final merged result.
    pub fn set_filter(&mut self, filter: &str) -> Result<()> {
        let c_filter = to_cstring(filter)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_multi_query_set_filter(self.handle, c_filter.as_ptr())
        })
    }

    /// Sets whether vector data is returned in the result documents.
    pub fn set_include_vector(&mut self, include: bool) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_multi_query_set_include_vector(self.handle, include)
        })
    }

    /// Returns whether vector data is included in the result documents.
    pub fn include_vector(&self) -> bool {
        unsafe { zvec_rust_sys::zvec_multi_query_get_include_vector(self.handle) }
    }

    /// Sets the output field whitelist.
    pub fn set_output_fields(&mut self, fields: &[&str]) -> Result<()> {
        if fields.is_empty() {
            return Ok(());
        }
        let c_fields: Vec<_> = fields
            .iter()
            .map(|f| to_cstring(f))
            .collect::<Result<Vec<_>>>()?;
        let c_ptrs: Vec<_> = c_fields.iter().map(|s| s.as_ptr()).collect();
        check_error(unsafe {
            zvec_rust_sys::zvec_multi_query_set_output_fields(
                self.handle,
                c_ptrs.as_ptr(),
                c_ptrs.len(),
            )
        })
    }

    /// Configures Reciprocal Rank Fusion (RRF) as the rerank strategy.
    pub fn set_rerank_rrf(&mut self, rank_constant: i32) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_multi_query_set_rerank_rrf(self.handle, rank_constant)
        })
    }

    /// Configures a weighted rerank strategy with the given per-sub-query weights.
    pub fn set_rerank_weighted(&mut self, weights: &[f64]) -> Result<()> {
        if weights.is_empty() {
            return Err(Error {
                code: ErrorCode::InvalidArgument,
                message: "weights cannot be empty".into(),
            });
        }
        check_error(unsafe {
            zvec_rust_sys::zvec_multi_query_set_rerank_weighted(
                self.handle,
                weights.as_ptr(),
                weights.len(),
            )
        })
    }
}

impl Drop for MultiQuery {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_multi_query_destroy(self.handle) };
        }
    }
}

// Safety: MultiQuery owns its handle exclusively.
unsafe impl Send for MultiQuery {}

/// A sub-query inside a [`MultiQuery`].
///
/// Each sub-query targets a single vector or sparse-vector field.
pub struct SubQuery {
    pub(crate) handle: *mut zvec_rust_sys::zvec_sub_query_t,
}

impl SubQuery {
    /// Creates a new sub-query.
    pub fn new() -> Result<Self> {
        let handle = unsafe { zvec_rust_sys::zvec_sub_query_create() };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create sub-query".into(),
            });
        }
        Ok(SubQuery { handle })
    }

    /// Returns the raw FFI handle.
    ///
    /// # Safety
    /// The caller must not use the handle after the `SubQuery` is dropped.
    pub unsafe fn as_raw(&self) -> *mut zvec_rust_sys::zvec_sub_query_t {
        self.handle
    }

    /// Sets the number of candidates to retrieve before reranking.
    pub fn set_num_candidates(&mut self, n: i32) -> Result<()> {
        check_error(unsafe { zvec_rust_sys::zvec_sub_query_set_num_candidates(self.handle, n) })
    }

    /// Returns the number of candidates.
    pub fn num_candidates(&self) -> i32 {
        unsafe { zvec_rust_sys::zvec_sub_query_get_num_candidates(self.handle) }
    }

    /// Sets the target field name.
    pub fn set_field_name(&mut self, name: &str) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_sub_query_set_field_name(self.handle, c_name.as_ptr())
        })
    }

    /// Sets the dense query vector (f32).
    pub fn set_query_vector(&mut self, data: &[f32]) -> Result<()> {
        if data.is_empty() {
            return Err(Error {
                code: ErrorCode::InvalidArgument,
                message: "query vector cannot be empty".into(),
            });
        }
        let bytes = std::mem::size_of_val(data);
        check_error(unsafe {
            zvec_rust_sys::zvec_sub_query_set_query_vector(
                self.handle,
                data.as_ptr() as *const c_void,
                bytes,
            )
        })
    }

    /// Sets the sparse vector (indices + values, equal length).
    pub fn set_sparse_vector(&mut self, indices: &[u32], values: &[f32]) -> Result<()> {
        if indices.len() != values.len() {
            return Err(Error {
                code: ErrorCode::InvalidArgument,
                message: "indices and values must have the same length".into(),
            });
        }
        if indices.is_empty() {
            return Err(Error {
                code: ErrorCode::InvalidArgument,
                message: "sparse vector cannot be empty".into(),
            });
        }
        check_error(unsafe {
            zvec_rust_sys::zvec_sub_query_set_sparse_vector(
                self.handle,
                indices.as_ptr(),
                values.as_ptr(),
                indices.len(),
            )
        })
    }

    /// Sets only the sparse-vector indices.
    pub fn set_sparse_indices(&mut self, indices: &[u32]) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_sub_query_set_sparse_indices(
                self.handle,
                indices.as_ptr(),
                indices.len(),
            )
        })
    }

    /// Sets only the sparse-vector values.
    pub fn set_sparse_values(&mut self, values: &[f32]) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_sub_query_set_sparse_values(
                self.handle,
                values.as_ptr(),
                values.len(),
            )
        })
    }

    /// Sets HNSW query parameters (takes ownership on success).
    pub fn set_hnsw_params(&mut self, mut params: HnswQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_sub_query_set_hnsw_params(self.handle, params.handle)
        })?;
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets IVF query parameters (takes ownership on success).
    pub fn set_ivf_params(&mut self, mut params: IvfQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_sub_query_set_ivf_params(self.handle, params.handle)
        })?;
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets IVF RaBitQ query parameters (takes ownership on success).
    pub fn set_ivf_rabitq_params(&mut self, mut params: IvfRabitqQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_sub_query_set_ivf_rabitq_params(self.handle, params.handle)
        })?;
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets Flat query parameters (takes ownership on success).
    pub fn set_flat_params(&mut self, mut params: FlatQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_sub_query_set_flat_params(self.handle, params.handle)
        })?;
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets DiskANN query parameters (takes ownership on success).
    pub fn set_diskann_params(&mut self, mut params: DiskannQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_sub_query_set_diskann_params(self.handle, params.handle)
        })?;
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets FTS query parameters (takes ownership on success).
    pub fn set_fts_params(&mut self, mut params: FtsQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_sub_query_set_fts_params(self.handle, params.handle)
        })?;
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets FTS payload (payload is copied, caller retains ownership).
    pub fn set_fts(&mut self, fts: &Fts) -> Result<()> {
        check_error(unsafe { zvec_rust_sys::zvec_sub_query_set_fts(self.handle, fts.handle) })
    }
}

impl Drop for SubQuery {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_sub_query_destroy(self.handle) };
        }
    }
}

// Safety: SubQuery owns its handle exclusively.
unsafe impl Send for SubQuery {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_drop_multi_query() {
        let mq = MultiQuery::new().expect("create multi-query");
        assert_eq!(mq.sub_query_count(), 0);
    }

    #[test]
    fn create_and_drop_sub_query() {
        let _sq = SubQuery::new().expect("create sub-query");
    }

    #[test]
    fn multi_query_basic_setters() {
        let mut mq = MultiQuery::new().expect("create multi-query");
        mq.set_topk(20).expect("set topk");
        assert_eq!(mq.topk(), 20);
        mq.set_include_vector(true).expect("set include_vector");
        assert!(mq.include_vector());
    }

    #[test]
    fn add_sub_query_increments_count() {
        let mut mq = MultiQuery::new().expect("create multi-query");
        let mut sq = SubQuery::new().expect("create sub-query");
        sq.set_field_name("vec").expect("set field name");
        sq.set_query_vector(&[0.1, 0.2, 0.3, 0.4])
            .expect("set query vector");
        sq.set_num_candidates(50).expect("set num candidates");
        mq.add_sub_query(&sq).expect("add sub-query");
        assert_eq!(mq.sub_query_count(), 1);
    }

    #[test]
    fn rerank_weighted_rejects_empty() {
        let mut mq = MultiQuery::new().expect("create multi-query");
        let err = mq.set_rerank_weighted(&[]).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn sub_query_set_fts() {
        let mut sub = SubQuery::new().expect("create sub-query");
        sub.set_field_name("content").expect("set field name");

        let mut fts = Fts::new().expect("create fts payload");
        fts.set_match_string("hello world")
            .expect("set match string");

        sub.set_fts(&fts).expect("set fts payload");
    }

    #[test]
    fn sub_query_set_fts_params() {
        let mut sub = SubQuery::new().expect("create sub-query");
        sub.set_field_name("content").expect("set field name");

        let params = FtsQueryParams::new(Some("AND")).expect("create fts params");
        sub.set_fts_params(params).expect("set fts params");
    }

    #[test]
    fn sub_query_set_ivf_rabitq_params() {
        let mut sub = SubQuery::new().expect("create sub-query");
        sub.set_field_name("embedding").expect("set field name");
        sub.set_query_vector(&[0.1, 0.2, 0.3, 0.4])
            .expect("set query vector");

        let params = IvfRabitqQueryParams::new(16, 0.0, false, false);
        sub.set_ivf_rabitq_params(params)
            .expect("set ivf rabitq params");
    }

    #[test]
    fn sub_query_set_diskann_params() {
        let mut sub = SubQuery::new().expect("create sub-query");
        sub.set_field_name("embedding").expect("set field name");
        sub.set_query_vector(&[0.1, 0.2, 0.3, 0.4])
            .expect("set query vector");

        let params = DiskannQueryParams::new(200);
        sub.set_diskann_params(params).expect("set diskann params");
    }
}
