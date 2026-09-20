use std::os::raw::c_void;

use crate::error::{check_error, to_cstring, Error, ErrorCode, Result};

/// HNSW-specific query parameters.
pub struct HnswQueryParams {
    pub(crate) handle: *mut zvec_rust_sys::zvec_hnsw_query_params_t,
}

impl HnswQueryParams {
    /// Creates new HNSW query parameters.
    pub fn new(ef: i32, radius: f32, is_linear: bool, is_using_refiner: bool) -> Self {
        let handle = unsafe {
            zvec_rust_sys::zvec_query_params_hnsw_create(ef, radius, is_linear, is_using_refiner)
        };
        HnswQueryParams { handle }
    }

    /// Sets the exploration factor.
    pub fn set_ef(&mut self, ef: i32) -> Result<()> {
        check_error(unsafe { zvec_rust_sys::zvec_query_params_hnsw_set_ef(self.handle, ef) })
    }

    /// Returns the exploration factor.
    pub fn ef(&self) -> i32 {
        unsafe { zvec_rust_sys::zvec_query_params_hnsw_get_ef(self.handle) }
    }
}

impl Drop for HnswQueryParams {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_query_params_hnsw_destroy(self.handle) };
        }
    }
}

/// IVF-specific query parameters.
pub struct IvfQueryParams {
    pub(crate) handle: *mut zvec_rust_sys::zvec_ivf_query_params_t,
}

impl IvfQueryParams {
    /// Creates new IVF query parameters.
    pub fn new(nprobe: i32, is_using_refiner: bool, scale_factor: f32) -> Self {
        let handle = unsafe {
            zvec_rust_sys::zvec_query_params_ivf_create(nprobe, is_using_refiner, scale_factor)
        };
        IvfQueryParams { handle }
    }

    /// Sets the number of probe clusters.
    pub fn set_nprobe(&mut self, nprobe: i32) -> Result<()> {
        check_error(unsafe { zvec_rust_sys::zvec_query_params_ivf_set_nprobe(self.handle, nprobe) })
    }

    /// Returns the number of probe clusters.
    pub fn nprobe(&self) -> i32 {
        unsafe { zvec_rust_sys::zvec_query_params_ivf_get_nprobe(self.handle) }
    }
}

impl Drop for IvfQueryParams {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_query_params_ivf_destroy(self.handle) };
        }
    }
}

/// IVF RaBitQ-specific query parameters.
pub struct IvfRabitqQueryParams {
    pub(crate) handle: *mut zvec_rust_sys::zvec_ivf_rabitq_query_params_t,
}

impl IvfRabitqQueryParams {
    /// Creates new IVF RaBitQ query parameters.
    pub fn new(nprobe: i32, radius: f32, is_linear: bool, is_using_refiner: bool) -> Self {
        let handle = unsafe {
            zvec_rust_sys::zvec_query_params_ivf_rabitq_create(
                nprobe,
                radius,
                is_linear,
                is_using_refiner,
            )
        };
        IvfRabitqQueryParams { handle }
    }

    /// Sets the number of probe clusters.
    pub fn set_nprobe(&mut self, nprobe: i32) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_query_params_ivf_rabitq_set_nprobe(self.handle, nprobe)
        })
    }

    /// Returns the number of probe clusters.
    pub fn nprobe(&self) -> i32 {
        unsafe { zvec_rust_sys::zvec_query_params_ivf_rabitq_get_nprobe(self.handle) }
    }

    /// Sets the candidate expansion factor used by the refiner.
    pub fn set_scale_factor(&mut self, scale_factor: f32) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_query_params_ivf_rabitq_set_scale_factor(self.handle, scale_factor)
        })
    }

    /// Returns the candidate expansion factor used by the refiner.
    pub fn scale_factor(&self) -> f32 {
        unsafe { zvec_rust_sys::zvec_query_params_ivf_rabitq_get_scale_factor(self.handle) }
    }
}

impl Drop for IvfRabitqQueryParams {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_query_params_ivf_rabitq_destroy(self.handle) };
        }
    }
}

/// Flat-specific query parameters.
pub struct FlatQueryParams {
    pub(crate) handle: *mut zvec_rust_sys::zvec_flat_query_params_t,
}

impl FlatQueryParams {
    /// Creates new Flat query parameters.
    pub fn new(is_using_refiner: bool, scale_factor: f32) -> Self {
        let handle =
            unsafe { zvec_rust_sys::zvec_query_params_flat_create(is_using_refiner, scale_factor) };
        FlatQueryParams { handle }
    }
}

impl Drop for FlatQueryParams {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_query_params_flat_destroy(self.handle) };
        }
    }
}

/// DiskANN-specific query parameters.
pub struct DiskannQueryParams {
    pub(crate) handle: *mut zvec_rust_sys::zvec_diskann_query_params_t,
}

impl DiskannQueryParams {
    /// Creates new DiskANN query parameters.
    ///
    /// - `list_size`: search frontier size (default in the C library: 300)
    pub fn new(list_size: i32) -> Self {
        let handle = unsafe { zvec_rust_sys::zvec_query_params_diskann_create(list_size) };
        DiskannQueryParams { handle }
    }

    /// Sets the search-time frontier size.
    pub fn set_list_size(&mut self, list_size: i32) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_query_params_diskann_set_list_size(self.handle, list_size)
        })
    }

    /// Returns the search-time frontier size.
    pub fn list_size(&self) -> i32 {
        unsafe { zvec_rust_sys::zvec_query_params_diskann_get_list_size(self.handle) }
    }
}

impl Drop for DiskannQueryParams {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_query_params_diskann_destroy(self.handle) };
        }
    }
}

/// FTS-specific query parameters controlling the default boolean operator.
pub struct FtsQueryParams {
    pub(crate) handle: *mut zvec_rust_sys::zvec_fts_query_params_t,
}

impl FtsQueryParams {
    /// Creates new FTS query parameters.
    ///
    /// `default_operator` sets the boolean operator for adjacent bare terms
    /// ("OR" or "AND", case-insensitive). Pass `None` to use the library default.
    pub fn new(default_operator: Option<&str>) -> Result<Self> {
        let c_op = default_operator.map(to_cstring).transpose()?;
        let handle = unsafe {
            zvec_rust_sys::zvec_query_params_fts_create(
                c_op.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            )
        };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create FTS query params".into(),
            });
        }
        Ok(FtsQueryParams { handle })
    }

    /// Sets the default boolean operator.
    pub fn set_default_operator(&mut self, op: &str) -> Result<()> {
        let c_op = to_cstring(op)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_query_params_fts_set_default_operator(self.handle, c_op.as_ptr())
        })
    }

    /// Returns the default boolean operator.
    pub fn default_operator(&self) -> Option<String> {
        unsafe {
            let ptr = zvec_rust_sys::zvec_query_params_fts_get_default_operator(self.handle);
            if ptr.is_null() {
                return None;
            }
            Some(std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    }
}

impl Drop for FtsQueryParams {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_query_params_fts_destroy(self.handle) };
        }
    }
}

/// FTS query payload holding the query expression and match string.
///
/// - `query_string`: a boolean / advanced query expression
/// - `match_string`: a natural-language match string
pub struct Fts {
    pub(crate) handle: *mut zvec_rust_sys::zvec_fts_t,
}

impl Fts {
    /// Creates a new FTS query payload.
    pub fn new() -> Result<Self> {
        let handle = unsafe { zvec_rust_sys::zvec_fts_create() };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create FTS payload".into(),
            });
        }
        Ok(Fts { handle })
    }

    /// Sets the boolean / advanced query expression.
    pub fn set_query_string(&mut self, query: &str) -> Result<()> {
        let c = to_cstring(query)?;
        check_error(unsafe { zvec_rust_sys::zvec_fts_set_query_string(self.handle, c.as_ptr()) })
    }

    /// Sets the natural-language match string.
    pub fn set_match_string(&mut self, match_str: &str) -> Result<()> {
        let c = to_cstring(match_str)?;
        check_error(unsafe { zvec_rust_sys::zvec_fts_set_match_string(self.handle, c.as_ptr()) })
    }

    /// Returns the query expression, or `None` if not set.
    pub fn query_string(&self) -> Option<String> {
        unsafe {
            let ptr = zvec_rust_sys::zvec_fts_get_query_string(self.handle);
            if ptr.is_null() {
                return None;
            }
            Some(std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    }

    /// Returns the match string, or `None` if not set.
    pub fn match_string(&self) -> Option<String> {
        unsafe {
            let ptr = zvec_rust_sys::zvec_fts_get_match_string(self.handle);
            if ptr.is_null() {
                return None;
            }
            Some(std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    }
}

impl Drop for Fts {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_fts_destroy(self.handle) };
        }
    }
}

/// A scalar filter, full-text, or vector similarity search query.
pub struct SearchQuery {
    pub(crate) handle: *mut zvec_rust_sys::zvec_vector_query_t,
}

impl SearchQuery {
    /// Creates a scalar query without a vector or full-text field.
    ///
    /// Use [`SearchQuery::set_filter`] to select documents and
    /// [`SearchQuery::set_output_fields`] to restrict the returned fields.
    /// Results are limited to `topk` and have no guaranteed ordering; callers
    /// enumerating all matches must account for that limit.
    pub fn scalar(topk: i32) -> Result<Self> {
        let handle = unsafe { zvec_rust_sys::zvec_vector_query_create() };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create scalar query".into(),
            });
        }

        // Own the handle before fallible setup so an error cannot leak it.
        let query = SearchQuery { handle };
        check_error(unsafe { zvec_rust_sys::zvec_vector_query_set_topk(query.handle, topk) })?;
        Ok(query)
    }

    /// Returns the raw FFI handle.
    ///
    /// # Safety
    /// The caller must not use the handle after the `SearchQuery` is dropped.
    pub unsafe fn as_raw(&self) -> *mut zvec_rust_sys::zvec_vector_query_t {
        self.handle
    }

    /// Creates a `SearchQuery` from a raw FFI handle.
    ///
    /// # Safety
    /// The caller must ensure the handle is valid and was created by the zvec C API.
    /// The `SearchQuery` takes ownership and will call `zvec_vector_query_destroy` on drop.
    pub unsafe fn from_raw(handle: *mut zvec_rust_sys::zvec_vector_query_t) -> Self {
        SearchQuery { handle }
    }

    /// Creates a new vector query with the given field name, query vector, and topk.
    pub fn new(field_name: &str, vector: &[f32], topk: i32) -> Result<Self> {
        let handle = unsafe { zvec_rust_sys::zvec_vector_query_create() };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create vector query".into(),
            });
        }

        let c_field = to_cstring(field_name)?;
        let query = SearchQuery { handle };

        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_field_name(query.handle, c_field.as_ptr())
        })?;
        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_query_vector(
                query.handle,
                vector.as_ptr() as *const c_void,
                std::mem::size_of_val(vector),
            )
        })?;
        check_error(unsafe { zvec_rust_sys::zvec_vector_query_set_topk(query.handle, topk) })?;

        Ok(query)
    }

    /// Creates a pure full-text (keyword-only) search query with no dense vector.
    ///
    /// [`SearchQuery::new`] and [`SearchQueryBuilder::build`] both require a query
    /// vector, so an FTS clause can only ever be layered on top of a dense vector
    /// (hybrid search). This constructor reaches the C API's vector-less FTS path:
    /// once an FTS payload is attached, the underlying `zvec_vector_query` does not
    /// require a query vector, enabling keyword-only retrieval.
    ///
    /// `field_name` must be the FTS-indexed field.
    pub fn fts(field_name: &str, fts: &Fts, topk: i32) -> Result<Self> {
        let handle = unsafe { zvec_rust_sys::zvec_vector_query_create() };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create vector query".into(),
            });
        }

        // Wrap the handle before any fallible call so `Drop` frees it on early return.
        let mut query = SearchQuery { handle };
        let c_field = to_cstring(field_name)?;

        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_field_name(query.handle, c_field.as_ptr())
        })?;
        check_error(unsafe { zvec_rust_sys::zvec_vector_query_set_topk(query.handle, topk) })?;
        query.set_fts(fts)?;

        Ok(query)
    }

    /// Returns a builder for constructing a search query.
    pub fn builder() -> SearchQueryBuilder {
        SearchQueryBuilder::new()
    }

    /// Sets the filter expression.
    pub fn set_filter(&mut self, filter: &str) -> Result<()> {
        let c_filter = to_cstring(filter)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_filter(self.handle, c_filter.as_ptr())
        })
    }

    /// Sets whether to include vector data in results.
    pub fn set_include_vector(&mut self, include: bool) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_include_vector(self.handle, include)
        })
    }

    /// Sets whether to include doc ID in results.
    pub fn set_include_doc_id(&mut self, include: bool) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_include_doc_id(self.handle, include)
        })
    }

    /// Sets the output fields to include in results.
    pub fn set_output_fields(&mut self, fields: &[&str]) -> Result<()> {
        let c_fields: Vec<_> = fields
            .iter()
            .map(|f| to_cstring(f))
            .collect::<Result<Vec<_>>>()?;
        let c_ptrs: Vec<_> = c_fields.iter().map(|f| f.as_ptr()).collect();
        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_output_fields(
                self.handle,
                c_ptrs.as_ptr(),
                c_ptrs.len(),
            )
        })
    }

    /// Sets HNSW query parameters (takes ownership on success).
    pub fn set_hnsw_params(&mut self, mut params: HnswQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_hnsw_params(self.handle, params.handle)
        })?;
        // Ownership transferred to query only on success; prevent double-free
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets IVF query parameters (takes ownership on success).
    pub fn set_ivf_params(&mut self, mut params: IvfQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_ivf_params(self.handle, params.handle)
        })?;
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets IVF RaBitQ query parameters (takes ownership on success).
    pub fn set_ivf_rabitq_params(&mut self, mut params: IvfRabitqQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_ivf_rabitq_params(self.handle, params.handle)
        })?;
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets Flat query parameters (takes ownership on success).
    pub fn set_flat_params(&mut self, mut params: FlatQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_flat_params(self.handle, params.handle)
        })?;
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets DiskANN query parameters (takes ownership on success).
    pub fn set_diskann_params(&mut self, mut params: DiskannQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_diskann_params(self.handle, params.handle)
        })?;
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets FTS query parameters (takes ownership on success).
    pub fn set_fts_params(&mut self, mut params: FtsQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_vector_query_set_fts_params(self.handle, params.handle)
        })?;
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets FTS payload (payload is copied, caller retains ownership).
    pub fn set_fts(&mut self, fts: &Fts) -> Result<()> {
        check_error(unsafe { zvec_rust_sys::zvec_vector_query_set_fts(self.handle, fts.handle) })
    }
}

impl Drop for SearchQuery {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_vector_query_destroy(self.handle) };
        }
    }
}

/// Builder for constructing a [`SearchQuery`].
pub struct SearchQueryBuilder {
    field_name: Option<String>,
    vector: Option<Vec<f32>>,
    topk: i32,
    filter: Option<String>,
    include_vector: Option<bool>,
    include_doc_id: Option<bool>,
    output_fields: Option<Vec<String>>,
    fts_query_string: Option<String>,
    fts_match_string: Option<String>,
}

impl SearchQueryBuilder {
    fn new() -> Self {
        SearchQueryBuilder {
            field_name: None,
            vector: None,
            topk: 10,
            filter: None,
            include_vector: None,
            include_doc_id: None,
            output_fields: None,
            fts_query_string: None,
            fts_match_string: None,
        }
    }

    /// Sets the field name to query.
    pub fn field_name(mut self, name: &str) -> Self {
        self.field_name = Some(name.to_string());
        self
    }

    /// Sets the query vector.
    pub fn vector(mut self, vector: &[f32]) -> Self {
        self.vector = Some(vector.to_vec());
        self
    }

    /// Sets the number of results to return.
    pub fn topk(mut self, topk: i32) -> Self {
        self.topk = topk;
        self
    }

    /// Sets the filter expression.
    pub fn filter(mut self, filter: &str) -> Self {
        self.filter = Some(filter.to_string());
        self
    }

    /// Sets whether to include vector data in results.
    pub fn include_vector(mut self, include: bool) -> Self {
        self.include_vector = Some(include);
        self
    }

    /// Sets whether to include doc ID in results.
    pub fn include_doc_id(mut self, include: bool) -> Self {
        self.include_doc_id = Some(include);
        self
    }

    /// Sets the output fields.
    pub fn output_fields(mut self, fields: &[&str]) -> Self {
        self.output_fields = Some(fields.iter().map(|s| s.to_string()).collect());
        self
    }

    /// Sets the FTS boolean / advanced query expression.
    pub fn fts_query_string(mut self, query: &str) -> Self {
        self.fts_query_string = Some(query.to_string());
        self
    }

    /// Sets the FTS natural-language match string.
    pub fn fts_match_string(mut self, match_str: &str) -> Self {
        self.fts_match_string = Some(match_str.to_string());
        self
    }

    /// Builds the search query.
    pub fn build(self) -> Result<SearchQuery> {
        let field_name = self.field_name.ok_or_else(|| Error {
            code: ErrorCode::InvalidArgument,
            message: "field_name is required".into(),
        })?;
        let vector = self.vector.ok_or_else(|| Error {
            code: ErrorCode::InvalidArgument,
            message: "vector is required".into(),
        })?;

        let mut query = SearchQuery::new(&field_name, &vector, self.topk)?;

        if let Some(filter) = &self.filter {
            query.set_filter(filter)?;
        }
        if let Some(include) = self.include_vector {
            query.set_include_vector(include)?;
        }
        if let Some(include) = self.include_doc_id {
            query.set_include_doc_id(include)?;
        }
        if let Some(fields) = &self.output_fields {
            let field_refs: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
            query.set_output_fields(&field_refs)?;
        }
        if self.fts_query_string.is_some() || self.fts_match_string.is_some() {
            let mut fts = Fts::new()?;
            if let Some(qs) = &self.fts_query_string {
                fts.set_query_string(qs)?;
            }
            if let Some(ms) = &self.fts_match_string {
                fts.set_match_string(ms)?;
            }
            query.set_fts(&fts)?;
        }

        Ok(query)
    }
}

/// A grouped vector similarity search query.
pub struct GroupBySearchQuery {
    pub(crate) handle: *mut zvec_rust_sys::zvec_group_by_vector_query_t,
}

impl GroupBySearchQuery {
    /// Creates a new group-by search query.
    pub fn new(
        field_name: &str,
        group_by_field: &str,
        vector: &[f32],
        group_count: u32,
        group_topk: u32,
    ) -> Result<Self> {
        let handle = unsafe { zvec_rust_sys::zvec_group_by_vector_query_create() };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create group by vector query".into(),
            });
        }

        let c_field = to_cstring(field_name)?;
        let c_group_field = to_cstring(group_by_field)?;

        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_field_name(handle, c_field.as_ptr())
        })?;
        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_group_by_field_name(
                handle,
                c_group_field.as_ptr(),
            )
        })?;
        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_query_vector(
                handle,
                vector.as_ptr() as *const c_void,
                std::mem::size_of_val(vector),
            )
        })?;
        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_group_count(handle, group_count)
        })?;
        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_topk_per_group(handle, group_topk)
        })?;

        Ok(GroupBySearchQuery { handle })
    }

    /// Sets the filter expression.
    pub fn set_filter(&mut self, filter: &str) -> Result<()> {
        let c_filter = to_cstring(filter)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_filter(self.handle, c_filter.as_ptr())
        })
    }

    /// Sets whether to include vector data in results.
    pub fn set_include_vector(&mut self, include: bool) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_include_vector(self.handle, include)
        })
    }

    /// Sets the output fields.
    pub fn set_output_fields(&mut self, fields: &[&str]) -> Result<()> {
        let c_fields: Vec<_> = fields
            .iter()
            .map(|f| to_cstring(f))
            .collect::<Result<Vec<_>>>()?;
        let c_ptrs: Vec<_> = c_fields.iter().map(|f| f.as_ptr()).collect();
        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_output_fields(
                self.handle,
                c_ptrs.as_ptr(),
                c_ptrs.len(),
            )
        })
    }

    /// Sets HNSW query parameters (takes ownership on success).
    pub fn set_hnsw_params(&mut self, mut params: HnswQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_hnsw_params(self.handle, params.handle)
        })?;
        // Ownership transferred to query only on success; prevent double-free
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets IVF query parameters (takes ownership on success).
    pub fn set_ivf_params(&mut self, mut params: IvfQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_ivf_params(self.handle, params.handle)
        })?;
        // Ownership transferred to query only on success; prevent double-free
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets IVF RaBitQ query parameters (takes ownership on success).
    pub fn set_ivf_rabitq_params(&mut self, mut params: IvfRabitqQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_ivf_rabitq_params(
                self.handle,
                params.handle,
            )
        })?;
        // Ownership transferred to query only on success; prevent double-free
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets Flat query parameters (takes ownership on success).
    pub fn set_flat_params(&mut self, mut params: FlatQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_flat_params(self.handle, params.handle)
        })?;
        // Ownership transferred to query only on success; prevent double-free
        params.handle = std::ptr::null_mut();
        Ok(())
    }

    /// Sets DiskANN query parameters (takes ownership on success).
    pub fn set_diskann_params(&mut self, mut params: DiskannQueryParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_group_by_vector_query_set_diskann_params(self.handle, params.handle)
        })?;
        // Ownership transferred to query only on success; prevent double-free
        params.handle = std::ptr::null_mut();
        Ok(())
    }
}

impl Drop for GroupBySearchQuery {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_group_by_vector_query_destroy(self.handle) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vector_query_builder_default_values() {
        let builder = SearchQueryBuilder::new();
        assert!(builder.field_name.is_none());
        assert!(builder.vector.is_none());
        assert_eq!(builder.topk, 10);
        assert!(builder.filter.is_none());
        assert!(builder.include_vector.is_none());
        assert!(builder.include_doc_id.is_none());
        assert!(builder.output_fields.is_none());
    }

    #[test]
    fn test_vector_query_builder_setters() {
        let builder = SearchQueryBuilder::new()
            .field_name("test_field")
            .vector(&[1.0, 2.0, 3.0])
            .topk(5)
            .filter("age > 18")
            .include_vector(true)
            .include_doc_id(false)
            .output_fields(&["name", "age"]);

        assert_eq!(builder.field_name, Some("test_field".to_string()));
        assert_eq!(builder.vector, Some(vec![1.0, 2.0, 3.0]));
        assert_eq!(builder.topk, 5);
        assert_eq!(builder.filter, Some("age > 18".to_string()));
        assert_eq!(builder.include_vector, Some(true));
        assert_eq!(builder.include_doc_id, Some(false));
        assert_eq!(
            builder.output_fields,
            Some(vec!["name".to_string(), "age".to_string()])
        );
    }

    #[test]
    fn test_vector_query_builder_build_missing_field_name() {
        let builder = SearchQueryBuilder::new().vector(&[1.0, 2.0, 3.0]);

        let result = builder.build();
        assert!(result.is_err());
        if let Err(e) = result {
            assert_eq!(e.code, ErrorCode::InvalidArgument);
            assert!(e.message.contains("field_name is required"));
        }
    }

    #[test]
    fn test_vector_query_builder_build_missing_vector() {
        let builder = SearchQueryBuilder::new().field_name("test_field");

        let result = builder.build();
        assert!(result.is_err());
        if let Err(e) = result {
            assert_eq!(e.code, ErrorCode::InvalidArgument);
            assert!(e.message.contains("vector is required"));
        }
    }

    #[test]
    fn test_vector_query_builder_builder_method() {
        let builder = SearchQuery::builder();
        assert!(builder.field_name.is_none());
        assert!(builder.vector.is_none());
        assert_eq!(builder.topk, 10);
    }

    #[test]
    fn test_vector_query_builder_partial_setters() {
        let builder = SearchQueryBuilder::new()
            .field_name("test_field")
            .vector(&[1.0, 2.0])
            .filter("status = 'active'");
        assert_eq!(builder.field_name, Some("test_field".to_string()));
        assert_eq!(builder.vector, Some(vec![1.0, 2.0]));
        assert_eq!(builder.filter, Some("status = 'active'".to_string()));
        assert!(builder.include_vector.is_none());
        assert!(builder.output_fields.is_none());
    }

    #[test]
    fn test_vector_query_builder_empty_vector() {
        let builder = SearchQueryBuilder::new()
            .field_name("test_field")
            .vector(&[]);
        assert_eq!(builder.vector, Some(vec![]));
    }

    #[test]
    fn test_vector_query_builder_empty_output_fields() {
        let builder = SearchQueryBuilder::new()
            .field_name("test_field")
            .vector(&[1.0])
            .output_fields(&[]);
        assert_eq!(builder.output_fields, Some(vec![]));
    }

    #[test]
    fn test_vector_query_builder_topk_zero() {
        let builder = SearchQueryBuilder::new()
            .field_name("test_field")
            .vector(&[1.0])
            .topk(0);
        assert_eq!(builder.topk, 0);
    }

    #[test]
    fn test_vector_query_builder_topk_negative() {
        let builder = SearchQueryBuilder::new()
            .field_name("test_field")
            .vector(&[1.0])
            .topk(-1);
        assert_eq!(builder.topk, -1);
    }

    #[test]
    fn test_vector_query_builder_overwrite_field_name() {
        let builder = SearchQueryBuilder::new()
            .field_name("first_field")
            .field_name("second_field");
        assert_eq!(builder.field_name, Some("second_field".to_string()));
    }

    #[test]
    fn test_vector_query_builder_large_vector() {
        let large_vector: Vec<f32> = (0..1024).map(|i| i as f32).collect();
        let builder = SearchQueryBuilder::new()
            .field_name("test_field")
            .vector(&large_vector);
        assert_eq!(builder.vector.as_ref().unwrap().len(), 1024);
    }

    #[test]
    fn test_fts_query_no_vector() {
        // A pure FTS query must build without a query vector; the builder path
        // (which requires `vector`) cannot express this.
        let mut fts = Fts::new().expect("create fts payload");
        fts.set_match_string("hello world")
            .expect("set match string");

        let query = SearchQuery::fts("content", &fts, 10);
        assert!(
            query.is_ok(),
            "pure FTS query should build without a vector"
        );
        assert!(!unsafe { query.unwrap().as_raw() }.is_null());
    }

    #[test]
    fn test_diskann_query_params_create_and_getters() {
        let mut params = DiskannQueryParams::new(200);
        assert_eq!(params.list_size(), 200);
        params.set_list_size(300).expect("set list_size");
        assert_eq!(params.list_size(), 300);
    }

    #[test]
    fn test_ivf_rabitq_query_params_create_and_getters() {
        let mut params = IvfRabitqQueryParams::new(16, 0.0, false, true);
        assert_eq!(params.nprobe(), 16);
        params.set_nprobe(32).expect("set nprobe");
        assert_eq!(params.nprobe(), 32);
        params.set_scale_factor(2.5).expect("set scale_factor");
        assert!((params.scale_factor() - 2.5).abs() < f32::EPSILON);
    }
}
