use std::ffi::CStr;

use crate::error::{check_error, to_cstring, Error, ErrorCode, Result};
use crate::types::{DataType, IndexType, MetricType, QuantizeType};

/// Parameters for configuring an index on a field.
pub struct IndexParams {
    pub(crate) handle: *mut zvec_rust_sys::zvec_index_params_t,
    owned: bool,
}

impl IndexParams {
    /// Returns the raw FFI handle.
    ///
    /// # Safety
    /// The caller must not use the handle after the `IndexParams` is dropped.
    pub unsafe fn as_raw(&self) -> *mut zvec_rust_sys::zvec_index_params_t {
        self.handle
    }

    /// Creates HNSW index parameters.
    pub fn hnsw(metric: MetricType, m: i32, ef_construction: i32) -> Result<Self> {
        unsafe {
            let handle = zvec_rust_sys::zvec_index_params_create(IndexType::Hnsw as u32);
            if handle.is_null() {
                return Err(Error {
                    code: ErrorCode::InternalError,
                    message: "failed to create HNSW index params".into(),
                });
            }
            check_error(zvec_rust_sys::zvec_index_params_set_metric_type(
                handle,
                metric as u32,
            ))?;
            check_error(zvec_rust_sys::zvec_index_params_set_hnsw_params(
                handle,
                m,
                ef_construction,
            ))?;
            Ok(IndexParams {
                handle,
                owned: true,
            })
        }
    }

    /// Creates HNSW index parameters with quantization.
    pub fn hnsw_with_quantize(
        metric: MetricType,
        m: i32,
        ef_construction: i32,
        quantize: QuantizeType,
    ) -> Result<Self> {
        unsafe {
            let handle = zvec_rust_sys::zvec_index_params_create(IndexType::Hnsw as u32);
            if handle.is_null() {
                return Err(Error {
                    code: ErrorCode::InternalError,
                    message: "failed to create HNSW index params".into(),
                });
            }
            check_error(zvec_rust_sys::zvec_index_params_set_metric_type(
                handle,
                metric as u32,
            ))?;
            check_error(zvec_rust_sys::zvec_index_params_set_hnsw_params(
                handle,
                m,
                ef_construction,
            ))?;
            check_error(zvec_rust_sys::zvec_index_params_set_quantize_type(
                handle,
                quantize as u32,
            ))?;
            Ok(IndexParams {
                handle,
                owned: true,
            })
        }
    }

    /// Creates IVF index parameters.
    pub fn ivf(metric: MetricType, n_list: i32, n_iters: i32, use_soar: bool) -> Result<Self> {
        unsafe {
            let handle = zvec_rust_sys::zvec_index_params_create(IndexType::Ivf as u32);
            if handle.is_null() {
                return Err(Error {
                    code: ErrorCode::InternalError,
                    message: "failed to create IVF index params".into(),
                });
            }
            check_error(zvec_rust_sys::zvec_index_params_set_metric_type(
                handle,
                metric as u32,
            ))?;
            check_error(zvec_rust_sys::zvec_index_params_set_ivf_params(
                handle, n_list, n_iters, use_soar,
            ))?;
            Ok(IndexParams {
                handle,
                owned: true,
            })
        }
    }

    /// Creates IVF RaBitQ index parameters.
    ///
    /// IVF RaBitQ combines inverted-file clustering with RaBitQ quantization for
    /// memory-efficient approximate search.
    ///
    /// - `nlist`: number of cluster centers
    /// - `total_bits`: total bits for RaBitQ quantization
    /// - `sample_count`: sample count for training; `0` uses all vectors
    pub fn ivf_rabitq(
        metric: MetricType,
        nlist: i32,
        total_bits: i32,
        sample_count: i32,
    ) -> Result<Self> {
        unsafe {
            let handle = zvec_rust_sys::zvec_index_params_create(IndexType::IvfRabitq as u32);
            if handle.is_null() {
                return Err(Error {
                    code: ErrorCode::InternalError,
                    message: "failed to create IVF RaBitQ index params".into(),
                });
            }
            check_error(zvec_rust_sys::zvec_index_params_set_metric_type(
                handle,
                metric as u32,
            ))?;
            check_error(zvec_rust_sys::zvec_index_params_set_ivf_rabitq_params(
                handle,
                nlist,
                total_bits,
                sample_count,
            ))?;
            Ok(IndexParams {
                handle,
                owned: true,
            })
        }
    }

    /// Creates Flat index parameters.
    pub fn flat(metric: MetricType) -> Result<Self> {
        unsafe {
            let handle = zvec_rust_sys::zvec_index_params_create(IndexType::Flat as u32);
            if handle.is_null() {
                return Err(Error {
                    code: ErrorCode::InternalError,
                    message: "failed to create Flat index params".into(),
                });
            }
            check_error(zvec_rust_sys::zvec_index_params_set_metric_type(
                handle,
                metric as u32,
            ))?;
            Ok(IndexParams {
                handle,
                owned: true,
            })
        }
    }

    /// Creates DiskANN index parameters.
    ///
    /// DiskANN is a disk-based graph index that keeps the bulk of the index on
    /// disk, drastically reducing memory usage for large datasets.
    ///
    /// - `max_degree`: graph connectivity, i.e. max out-degree of the graph (typical: 64)
    /// - `list_size`: build-time candidate list size during construction (typical: 100)
    /// - `pq_chunk_num`: PQ chunk count; `0` disables product quantization
    pub fn diskann(
        metric: MetricType,
        max_degree: i32,
        list_size: i32,
        pq_chunk_num: i32,
    ) -> Result<Self> {
        unsafe {
            let handle = zvec_rust_sys::zvec_index_params_create(IndexType::Diskann as u32);
            if handle.is_null() {
                return Err(Error {
                    code: ErrorCode::InternalError,
                    message: "failed to create DiskANN index params".into(),
                });
            }
            check_error(zvec_rust_sys::zvec_index_params_set_metric_type(
                handle,
                metric as u32,
            ))?;
            check_error(zvec_rust_sys::zvec_index_params_set_diskann_params(
                handle,
                max_degree,
                list_size,
                pq_chunk_num,
            ))?;
            Ok(IndexParams {
                handle,
                owned: true,
            })
        }
    }

    /// Creates inverted index parameters for scalar fields.
    pub fn invert(enable_range_opt: bool, enable_wildcard: bool) -> Result<Self> {
        unsafe {
            let handle = zvec_rust_sys::zvec_index_params_create(IndexType::Invert as u32);
            if handle.is_null() {
                return Err(Error {
                    code: ErrorCode::InternalError,
                    message: "failed to create Invert index params".into(),
                });
            }
            check_error(zvec_rust_sys::zvec_index_params_set_invert_params(
                handle,
                enable_range_opt,
                enable_wildcard,
            ))?;
            Ok(IndexParams {
                handle,
                owned: true,
            })
        }
    }

    /// Creates FTS (Full-Text Search) index parameters.
    ///
    /// All parameters are optional — passing `None` keeps the library default.
    pub fn fts(
        tokenizer_name: Option<&str>,
        filters: Option<&[&str]>,
        extra_params: Option<&str>,
    ) -> Result<Self> {
        unsafe {
            let handle = zvec_rust_sys::zvec_index_params_create(IndexType::Fts as u32);
            if handle.is_null() {
                return Err(Error {
                    code: ErrorCode::InternalError,
                    message: "failed to create FTS index params".into(),
                });
            }
            let c_tokenizer = tokenizer_name.map(to_cstring).transpose()?;
            let c_extra = extra_params.map(to_cstring).transpose()?;

            let filter_array = if let Some(f) = filters {
                let arr = zvec_rust_sys::zvec_string_array_create(f.len());
                for (i, s) in f.iter().enumerate() {
                    let cs = to_cstring(s)?;
                    zvec_rust_sys::zvec_string_array_add(arr, i, cs.as_ptr());
                }
                arr
            } else {
                std::ptr::null_mut()
            };

            let result = check_error(zvec_rust_sys::zvec_index_params_set_fts_params(
                handle,
                c_tokenizer
                    .as_ref()
                    .map_or(std::ptr::null(), |c| c.as_ptr()),
                filter_array as *const _,
                c_extra.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            ));

            if !filter_array.is_null() {
                zvec_rust_sys::zvec_string_array_destroy(filter_array);
            }
            result?;
            Ok(IndexParams {
                handle,
                owned: true,
            })
        }
    }

    /// Returns the index type.
    pub fn index_type(&self) -> IndexType {
        IndexType::from(unsafe { zvec_rust_sys::zvec_index_params_get_type(self.handle) })
    }

    /// Returns the metric type.
    pub fn metric_type(&self) -> MetricType {
        MetricType::from(unsafe { zvec_rust_sys::zvec_index_params_get_metric_type(self.handle) })
    }

    /// Returns the quantize type.
    pub fn quantize_type(&self) -> QuantizeType {
        QuantizeType::from(unsafe {
            zvec_rust_sys::zvec_index_params_get_quantize_type(self.handle)
        })
    }

    /// Sets the metric type.
    pub fn set_metric_type(&mut self, metric: MetricType) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_index_params_set_metric_type(self.handle, metric as u32)
        })
    }

    /// Sets the quantize type.
    pub fn set_quantize_type(&mut self, quantize: QuantizeType) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_index_params_set_quantize_type(self.handle, quantize as u32)
        })
    }
}

impl Drop for IndexParams {
    fn drop(&mut self) {
        if self.owned && !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_index_params_destroy(self.handle) };
        }
    }
}

/// Schema definition for a single field in a collection.
pub struct FieldSchema {
    pub(crate) handle: *mut zvec_rust_sys::zvec_field_schema_t,
    owned: bool,
}

impl FieldSchema {
    /// Creates a new field schema.
    ///
    /// - `name`: Field name
    /// - `data_type`: Data type of the field
    /// - `nullable`: Whether the field can be null
    /// - `dimension`: Vector dimension (0 for non-vector fields)
    pub fn new(name: &str, data_type: DataType, nullable: bool, dimension: u32) -> Result<Self> {
        let c_name = to_cstring(name)?;
        let handle = unsafe {
            zvec_rust_sys::zvec_field_schema_create(
                c_name.as_ptr(),
                data_type as u32,
                nullable,
                dimension,
            )
        };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create field schema".into(),
            });
        }
        Ok(FieldSchema {
            handle,
            owned: true,
        })
    }

    /// Creates a non-owning wrapper around an existing handle.
    #[allow(dead_code)]
    pub(crate) fn from_borrowed(handle: *mut zvec_rust_sys::zvec_field_schema_t) -> Self {
        FieldSchema {
            handle,
            owned: false,
        }
    }

    /// Sets the index parameters for this field.
    pub fn set_index_params(&mut self, params: &IndexParams) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_field_schema_set_index_params(self.handle, params.handle)
        })
    }

    /// Returns the field name.
    pub fn name(&self) -> &str {
        unsafe {
            let ptr = zvec_rust_sys::zvec_field_schema_get_name(self.handle);
            if ptr.is_null() {
                return "";
            }
            CStr::from_ptr(ptr).to_str().unwrap_or("")
        }
    }

    /// Returns the data type.
    pub fn data_type(&self) -> DataType {
        DataType::from(unsafe { zvec_rust_sys::zvec_field_schema_get_data_type(self.handle) })
    }

    /// Returns the dimension (for vector fields).
    pub fn dimension(&self) -> u32 {
        unsafe { zvec_rust_sys::zvec_field_schema_get_dimension(self.handle) }
    }

    /// Returns whether the field is nullable.
    pub fn is_nullable(&self) -> bool {
        unsafe { zvec_rust_sys::zvec_field_schema_is_nullable(self.handle) }
    }

    /// Returns whether this is a vector field.
    pub fn is_vector_field(&self) -> bool {
        unsafe { zvec_rust_sys::zvec_field_schema_is_vector_field(self.handle) }
    }

    /// Returns whether this is a dense vector field.
    pub fn is_dense_vector(&self) -> bool {
        unsafe { zvec_rust_sys::zvec_field_schema_is_dense_vector(self.handle) }
    }

    /// Returns whether this is a sparse vector field.
    pub fn is_sparse_vector(&self) -> bool {
        unsafe { zvec_rust_sys::zvec_field_schema_is_sparse_vector(self.handle) }
    }

    /// Returns whether this field has an index.
    pub fn has_index(&self) -> bool {
        unsafe { zvec_rust_sys::zvec_field_schema_has_index(self.handle) }
    }

    /// Returns the index type.
    pub fn index_type(&self) -> IndexType {
        IndexType::from(unsafe { zvec_rust_sys::zvec_field_schema_get_index_type(self.handle) })
    }

    /// Returns whether this is an array type.
    pub fn is_array_type(&self) -> bool {
        unsafe { zvec_rust_sys::zvec_field_schema_is_array_type(self.handle) }
    }
}

impl Drop for FieldSchema {
    fn drop(&mut self) {
        if self.owned && !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_field_schema_destroy(self.handle) };
        }
    }
}

/// Schema definition for a collection, containing field definitions.
pub struct CollectionSchema {
    pub(crate) handle: *mut zvec_rust_sys::zvec_collection_schema_t,
    owned: bool,
}

impl CollectionSchema {
    /// Creates a new collection schema with the given name.
    pub fn new(name: &str) -> Result<Self> {
        let c_name = to_cstring(name)?;
        let handle = unsafe { zvec_rust_sys::zvec_collection_schema_create(c_name.as_ptr()) };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create collection schema".into(),
            });
        }
        Ok(CollectionSchema {
            handle,
            owned: true,
        })
    }

    /// Returns a builder for constructing a collection schema.
    pub fn builder(name: &str) -> CollectionSchemaBuilder {
        CollectionSchemaBuilder::new(name)
    }

    /// Creates a non-owning wrapper around an existing handle.
    pub(crate) fn from_owned(handle: *mut zvec_rust_sys::zvec_collection_schema_t) -> Self {
        CollectionSchema {
            handle,
            owned: true,
        }
    }

    /// Adds a field to the schema.
    pub fn add_field(&mut self, field: &FieldSchema) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_schema_add_field(self.handle, field.handle)
        })
    }

    /// Returns the collection name.
    pub fn name(&self) -> &str {
        unsafe {
            let ptr = zvec_rust_sys::zvec_collection_schema_get_name(self.handle);
            if ptr.is_null() {
                return "";
            }
            CStr::from_ptr(ptr).to_str().unwrap_or("")
        }
    }

    /// Checks if a field exists in the schema.
    pub fn has_field(&self, name: &str) -> bool {
        let c_name = match to_cstring(name) {
            Ok(s) => s,
            Err(_) => return false,
        };
        unsafe { zvec_rust_sys::zvec_collection_schema_has_field(self.handle, c_name.as_ptr()) }
    }

    /// Checks if a field has an index.
    pub fn has_index(&self, field_name: &str) -> bool {
        let c_name = match to_cstring(field_name) {
            Ok(s) => s,
            Err(_) => return false,
        };
        unsafe { zvec_rust_sys::zvec_collection_schema_has_index(self.handle, c_name.as_ptr()) }
    }

    /// Drops a field from the schema.
    pub fn drop_field(&mut self, name: &str) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_schema_drop_field(self.handle, c_name.as_ptr())
        })
    }

    /// Adds an index to a field.
    pub fn add_index(&mut self, field_name: &str, params: &IndexParams) -> Result<()> {
        let c_name = to_cstring(field_name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_schema_add_index(
                self.handle,
                c_name.as_ptr(),
                params.handle,
            )
        })
    }

    /// Drops an index from a field.
    pub fn drop_index(&mut self, field_name: &str) -> Result<()> {
        let c_name = to_cstring(field_name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_schema_drop_index(self.handle, c_name.as_ptr())
        })
    }

    /// Sets the maximum document count per segment.
    pub fn set_max_doc_count_per_segment(&mut self, count: u64) -> Result<()> {
        check_error(unsafe {
            zvec_rust_sys::zvec_collection_schema_set_max_doc_count_per_segment(self.handle, count)
        })
    }

    /// Returns the maximum document count per segment.
    pub fn max_doc_count_per_segment(&self) -> u64 {
        unsafe { zvec_rust_sys::zvec_collection_schema_get_max_doc_count_per_segment(self.handle) }
    }
}

impl Drop for CollectionSchema {
    fn drop(&mut self) {
        if self.owned && !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_collection_schema_destroy(self.handle) };
        }
    }
}

/// Builder for constructing a [`CollectionSchema`] with a fluent API.
pub struct CollectionSchemaBuilder {
    name: String,
    fields: Vec<(FieldSchema, Option<IndexParams>)>,
    max_doc_count_per_segment: Option<u64>,
    deferred_error: Option<Error>,
}

impl CollectionSchemaBuilder {
    /// Creates a new builder with the given collection name.
    pub fn new(name: &str) -> Self {
        CollectionSchemaBuilder {
            name: name.to_string(),
            fields: Vec::new(),
            max_doc_count_per_segment: None,
            deferred_error: None,
        }
    }

    /// Adds a field to the schema.
    pub fn add_field(mut self, field: FieldSchema) -> Self {
        self.fields.push((field, None));
        self
    }

    /// Adds a vector field with index parameters.
    pub fn add_vector_field(
        self,
        name: &str,
        data_type: DataType,
        dimension: u32,
        index_params: IndexParams,
    ) -> Self {
        match FieldSchema::new(name, data_type, false, dimension) {
            Ok(field) => {
                let mut s = self;
                s.fields.push((field, Some(index_params)));
                s
            }
            Err(e) => {
                let mut s = self;
                s.deferred_error = Some(e);
                s
            }
        }
    }

    /// Adds a scalar field with an inverted index.
    pub fn add_indexed_field(
        self,
        name: &str,
        data_type: DataType,
        index_params: IndexParams,
    ) -> Self {
        match FieldSchema::new(name, data_type, false, 0) {
            Ok(field) => {
                let mut s = self;
                s.fields.push((field, Some(index_params)));
                s
            }
            Err(e) => {
                let mut s = self;
                s.deferred_error = Some(e);
                s
            }
        }
    }

    /// Sets the maximum document count per segment.
    pub fn max_doc_count_per_segment(mut self, count: u64) -> Self {
        self.max_doc_count_per_segment = Some(count);
        self
    }

    /// Builds the collection schema.
    pub fn build(self) -> Result<CollectionSchema> {
        if let Some(e) = self.deferred_error {
            return Err(e);
        }
        let mut schema = CollectionSchema::new(&self.name)?;

        for (mut field, index_params) in self.fields {
            if let Some(params) = &index_params {
                field.set_index_params(params)?;
            }
            schema.add_field(&field)?;
        }

        if let Some(count) = self.max_doc_count_per_segment {
            schema.set_max_doc_count_per_segment(count)?;
        }

        Ok(schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MetricType;

    #[test]
    fn test_collection_schema_builder_default_values() {
        let builder = CollectionSchemaBuilder::new("test_collection");
        assert_eq!(builder.name, "test_collection");
        assert!(builder.fields.is_empty());
        assert!(builder.max_doc_count_per_segment.is_none());
    }

    #[test]
    fn test_collection_schema_builder_add_field() {
        let field = FieldSchema::new("test_field", DataType::Int32, false, 0).unwrap();
        let builder = CollectionSchemaBuilder::new("test_collection").add_field(field);

        assert_eq!(builder.fields.len(), 1);
        assert_eq!(builder.fields[0].0.name(), "test_field");
        assert!(builder.fields[0].1.is_none());
    }

    #[test]
    fn test_collection_schema_builder_add_vector_field() {
        let index_params = IndexParams::hnsw(MetricType::L2, 16, 200).unwrap();
        let builder = CollectionSchemaBuilder::new("test_collection").add_vector_field(
            "vector",
            DataType::VectorFp32,
            128,
            index_params,
        );

        assert_eq!(builder.fields.len(), 1);
        assert_eq!(builder.fields[0].0.name(), "vector");
        assert!(builder.fields[0].1.is_some());
    }

    #[test]
    fn test_collection_schema_builder_add_indexed_field() {
        let index_params = IndexParams::invert(true, false).unwrap();
        let builder = CollectionSchemaBuilder::new("test_collection").add_indexed_field(
            "age",
            DataType::Int32,
            index_params,
        );

        assert_eq!(builder.fields.len(), 1);
        assert_eq!(builder.fields[0].0.name(), "age");
        assert!(builder.fields[0].1.is_some());
    }

    #[test]
    fn test_collection_schema_builder_max_doc_count_per_segment() {
        let builder =
            CollectionSchemaBuilder::new("test_collection").max_doc_count_per_segment(1000);

        assert_eq!(builder.max_doc_count_per_segment, Some(1000));
    }

    #[test]
    fn test_collection_schema_builder_multiple_fields() {
        let field1 = FieldSchema::new("id", DataType::Int64, false, 0).unwrap();
        let field2 = FieldSchema::new("name", DataType::String, false, 0).unwrap();
        let index_params = IndexParams::hnsw(MetricType::L2, 16, 200).unwrap();

        let builder = CollectionSchemaBuilder::new("test_collection")
            .add_field(field1)
            .add_field(field2)
            .add_vector_field("vector", DataType::VectorFp32, 128, index_params);

        assert_eq!(builder.fields.len(), 3);
        assert_eq!(builder.fields[0].0.name(), "id");
        assert_eq!(builder.fields[1].0.name(), "name");
        assert_eq!(builder.fields[2].0.name(), "vector");
    }

    #[test]
    fn test_index_params_hnsw() {
        let params = IndexParams::hnsw(MetricType::L2, 16, 200).unwrap();
        assert_eq!(params.index_type(), IndexType::Hnsw);
        assert_eq!(params.metric_type(), MetricType::L2);
    }

    #[test]
    fn test_index_params_hnsw_with_quantize() {
        let params =
            IndexParams::hnsw_with_quantize(MetricType::L2, 16, 200, QuantizeType::Int8).unwrap();
        assert_eq!(params.index_type(), IndexType::Hnsw);
        assert_eq!(params.metric_type(), MetricType::L2);
        assert_eq!(params.quantize_type(), QuantizeType::Int8);
    }

    #[test]
    fn test_index_params_ivf() {
        let params = IndexParams::ivf(MetricType::Ip, 100, 10, true).unwrap();
        assert_eq!(params.index_type(), IndexType::Ivf);
        assert_eq!(params.metric_type(), MetricType::Ip);
    }

    #[test]
    fn test_index_params_flat() {
        let params = IndexParams::flat(MetricType::Cosine).unwrap();
        assert_eq!(params.index_type(), IndexType::Flat);
        assert_eq!(params.metric_type(), MetricType::Cosine);
    }

    #[test]
    fn test_index_params_diskann() {
        let params = IndexParams::diskann(MetricType::L2, 32, 100, 0).unwrap();
        assert_eq!(params.index_type(), IndexType::Diskann);
        assert_eq!(params.metric_type(), MetricType::L2);
    }

    #[test]
    fn test_index_params_invert() {
        let params = IndexParams::invert(true, true).unwrap();
        assert_eq!(params.index_type(), IndexType::Invert);
    }

    #[test]
    fn test_field_schema_properties() {
        let field = FieldSchema::new("test_field", DataType::VectorFp32, true, 128).unwrap();
        assert_eq!(field.name(), "test_field");
        assert_eq!(field.data_type(), DataType::VectorFp32);
        assert_eq!(field.dimension(), 128);
        assert!(field.is_nullable());
    }

    #[test]
    fn test_collection_schema_builder_method() {
        let builder = CollectionSchema::builder("test_collection");
        assert_eq!(builder.name, "test_collection");
        assert!(builder.fields.is_empty());
    }

    #[test]
    fn test_field_schema_non_nullable() {
        let field = FieldSchema::new("test_field", DataType::Int32, false, 0).unwrap();
        assert!(!field.is_nullable());
    }

    #[test]
    fn test_field_schema_scalar_dimension_zero() {
        let field = FieldSchema::new("scalar_field", DataType::String, false, 0).unwrap();
        assert_eq!(field.dimension(), 0);
        assert!(!field.is_vector_field());
    }

    #[test]
    fn test_field_schema_new_with_null_byte() {
        let result = FieldSchema::new("invalid\0field", DataType::String, false, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_field_schema_new_success() {
        let result = FieldSchema::new("valid_field", DataType::Int64, true, 0);
        assert!(result.is_ok());
        let field = result.unwrap();
        assert_eq!(field.name(), "valid_field");
        assert_eq!(field.data_type(), DataType::Int64);
    }

    #[test]
    fn test_index_params_set_metric_type() {
        let mut params = IndexParams::hnsw(MetricType::L2, 16, 200).unwrap();
        params.set_metric_type(MetricType::Cosine).unwrap();
        assert_eq!(params.metric_type(), MetricType::Cosine);
    }

    #[test]
    fn test_index_params_set_quantize_type() {
        let mut params =
            IndexParams::hnsw_with_quantize(MetricType::L2, 16, 200, QuantizeType::Undefined)
                .unwrap();
        params.set_quantize_type(QuantizeType::Int8).unwrap();
        assert_eq!(params.quantize_type(), QuantizeType::Int8);
    }

    #[test]
    fn test_index_params_ivf_cosine() {
        let params = IndexParams::ivf(MetricType::Cosine, 100, 10, false).unwrap();
        assert_eq!(params.index_type(), IndexType::Ivf);
        assert_eq!(params.metric_type(), MetricType::Cosine);
    }

    #[test]
    fn test_index_params_flat_ip() {
        let params = IndexParams::flat(MetricType::Ip).unwrap();
        assert_eq!(params.index_type(), IndexType::Flat);
        assert_eq!(params.metric_type(), MetricType::Ip);
    }

    #[test]
    fn test_collection_schema_builder_chaining() {
        let vector_index = IndexParams::hnsw(MetricType::L2, 16, 200).unwrap();
        let scalar_index = IndexParams::invert(true, false).unwrap();

        let builder = CollectionSchemaBuilder::new("chained_collection")
            .add_field(FieldSchema::new("id", DataType::Int64, false, 0).unwrap())
            .add_vector_field("embedding", DataType::VectorFp32, 128, vector_index)
            .add_indexed_field("category", DataType::String, scalar_index)
            .max_doc_count_per_segment(5000);

        assert_eq!(builder.fields.len(), 3);
        assert_eq!(builder.max_doc_count_per_segment, Some(5000));
        assert_eq!(builder.fields[0].0.name(), "id");
        assert_eq!(builder.fields[1].0.name(), "embedding");
        assert_eq!(builder.fields[2].0.name(), "category");
    }
}
