use std::ffi::CStr;
use std::os::raw::c_void;

use crate::error::{check_error, to_cstring, Error, ErrorCode, Result};
use crate::types::DataType;

/// A document in a zvec collection.
///
/// Documents contain typed fields and are used for both writing data to
/// and reading data from collections.
pub struct Doc {
    pub(crate) handle: *mut zvec_rust_sys::zvec_doc_t,
    owned: bool,
}

impl Doc {
    /// Returns the raw FFI handle.
    ///
    /// # Safety
    /// The caller must not use the handle after the `Doc` is dropped.
    pub unsafe fn as_raw(&self) -> *mut zvec_rust_sys::zvec_doc_t {
        self.handle
    }

    /// Creates an owning `Doc` from a raw FFI handle.
    ///
    /// # Safety
    /// The caller must ensure the handle is valid and was created by the zvec C API.
    /// The `Doc` takes ownership and will call `zvec_doc_destroy` on drop.
    pub unsafe fn from_raw(handle: *mut zvec_rust_sys::zvec_doc_t) -> Self {
        Doc {
            handle,
            owned: true,
        }
    }

    /// Creates a new empty document.
    pub fn new() -> Result<Self> {
        let handle = unsafe { zvec_rust_sys::zvec_doc_create() };
        if handle.is_null() {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: "failed to create document".into(),
            });
        }
        Ok(Doc {
            handle,
            owned: true,
        })
    }

    /// Creates a non-owning wrapper around an existing handle.
    #[allow(dead_code)]
    pub(crate) fn from_borrowed(handle: *mut zvec_rust_sys::zvec_doc_t) -> Self {
        Doc {
            handle,
            owned: false,
        }
    }

    /// Sets the primary key.
    pub fn set_pk(&mut self, pk: &str) {
        let c_pk = to_cstring(pk).expect("pk must not contain null bytes");
        unsafe { zvec_rust_sys::zvec_doc_set_pk(self.handle, c_pk.as_ptr()) };
    }

    /// Returns the primary key, or `None` if not set.
    pub fn get_pk(&self) -> Option<&str> {
        unsafe {
            let ptr = zvec_rust_sys::zvec_doc_get_pk_pointer(self.handle);
            if ptr.is_null() {
                None
            } else {
                CStr::from_ptr(ptr).to_str().ok()
            }
        }
    }

    /// Returns the document score (set by query results).
    pub fn get_score(&self) -> f32 {
        unsafe { zvec_rust_sys::zvec_doc_get_score(self.handle) }
    }

    #[allow(dead_code)]
    pub(crate) fn get_doc_id(&self) -> u64 {
        unsafe { zvec_rust_sys::zvec_doc_get_doc_id(self.handle) }
    }

    /// Returns the number of fields in the document.
    pub fn field_count(&self) -> usize {
        unsafe { zvec_rust_sys::zvec_doc_get_field_count(self.handle) }
    }

    /// Returns whether the document is empty.
    pub fn is_empty(&self) -> bool {
        unsafe { zvec_rust_sys::zvec_doc_is_empty(self.handle) }
    }

    /// Returns whether the document contains the specified field.
    pub fn has_field(&self, name: &str) -> bool {
        let c_name = match to_cstring(name) {
            Ok(s) => s,
            Err(_) => return false,
        };
        unsafe { zvec_rust_sys::zvec_doc_has_field(self.handle, c_name.as_ptr()) }
    }

    /// Returns whether the specified field is null.
    pub fn is_field_null(&self, name: &str) -> bool {
        let c_name = match to_cstring(name) {
            Ok(s) => s,
            Err(_) => return false,
        };
        unsafe { zvec_rust_sys::zvec_doc_is_field_null(self.handle, c_name.as_ptr()) }
    }

    // =========================================================================
    // Field setters
    // =========================================================================

    /// Adds a string field.
    pub fn add_string(&mut self, name: &str, value: &str) -> Result<()> {
        let c_name = to_cstring(name)?;
        let c_value = to_cstring(value)?;
        // The C API `zvec_doc_add_field_by_value` is length-delimited: it stores
        // exactly `size` bytes. Use `as_bytes()` (without the trailing NUL) so the
        // stored value is the string itself; sending `as_bytes_with_nul()` would
        // append a NUL to the stored data and break exact string filters.
        let bytes = c_value.as_bytes();
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::String as u32,
                bytes.as_ptr() as *const c_void,
                bytes.len(),
            )
        })
    }

    /// Adds a boolean field.
    pub fn add_bool(&mut self, name: &str, value: bool) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::Bool as u32,
                &value as *const bool as *const c_void,
                std::mem::size_of::<bool>(),
            )
        })
    }

    /// Adds an i32 field.
    pub fn add_i32(&mut self, name: &str, value: i32) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::Int32 as u32,
                &value as *const i32 as *const c_void,
                std::mem::size_of::<i32>(),
            )
        })
    }

    /// Adds an i64 field.
    pub fn add_i64(&mut self, name: &str, value: i64) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::Int64 as u32,
                &value as *const i64 as *const c_void,
                std::mem::size_of::<i64>(),
            )
        })
    }

    /// Adds a u32 field.
    pub fn add_u32(&mut self, name: &str, value: u32) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::Uint32 as u32,
                &value as *const u32 as *const c_void,
                std::mem::size_of::<u32>(),
            )
        })
    }

    /// Adds a u64 field.
    pub fn add_u64(&mut self, name: &str, value: u64) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::Uint64 as u32,
                &value as *const u64 as *const c_void,
                std::mem::size_of::<u64>(),
            )
        })
    }

    /// Adds an f32 field.
    pub fn add_f32(&mut self, name: &str, value: f32) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::Float as u32,
                &value as *const f32 as *const c_void,
                std::mem::size_of::<f32>(),
            )
        })
    }

    /// Adds an f64 field.
    pub fn add_f64(&mut self, name: &str, value: f64) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::Double as u32,
                &value as *const f64 as *const c_void,
                std::mem::size_of::<f64>(),
            )
        })
    }

    /// Adds a dense FP32 vector field.
    pub fn add_vector_f32(&mut self, name: &str, vector: &[f32]) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::VectorFp32 as u32,
                vector.as_ptr() as *const c_void,
                std::mem::size_of_val(vector),
            )
        })
    }

    /// Adds a dense FP64 vector field.
    pub fn add_vector_f64(&mut self, name: &str, vector: &[f64]) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::VectorFp64 as u32,
                vector.as_ptr() as *const c_void,
                std::mem::size_of_val(vector),
            )
        })
    }

    /// Adds a binary (raw bytes) field.
    pub fn add_binary(&mut self, name: &str, value: &[u8]) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::Binary as u32,
                value.as_ptr() as *const c_void,
                value.len(),
            )
        })
    }

    /// Adds a dense INT8 vector field.
    pub fn add_vector_i8(&mut self, name: &str, vector: &[i8]) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::VectorInt8 as u32,
                vector.as_ptr() as *const c_void,
                std::mem::size_of_val(vector),
            )
        })
    }

    /// Adds a dense INT16 vector field.
    pub fn add_vector_i16(&mut self, name: &str, vector: &[i16]) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                DataType::VectorInt16 as u32,
                vector.as_ptr() as *const c_void,
                std::mem::size_of_val(vector),
            )
        })
    }

    // =========================================================================
    // Array field setters
    // =========================================================================

    fn add_typed_array<T>(&mut self, name: &str, data_type: DataType, values: &[T]) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_add_field_by_value(
                self.handle,
                c_name.as_ptr(),
                data_type as u32,
                values.as_ptr() as *const c_void,
                std::mem::size_of_val(values),
            )
        })
    }

    /// Adds an array of i32 values.
    pub fn add_array_i32(&mut self, name: &str, values: &[i32]) -> Result<()> {
        self.add_typed_array(name, DataType::ArrayInt32, values)
    }

    /// Adds an array of i64 values.
    pub fn add_array_i64(&mut self, name: &str, values: &[i64]) -> Result<()> {
        self.add_typed_array(name, DataType::ArrayInt64, values)
    }

    /// Adds an array of u32 values.
    pub fn add_array_u32(&mut self, name: &str, values: &[u32]) -> Result<()> {
        self.add_typed_array(name, DataType::ArrayUint32, values)
    }

    /// Adds an array of u64 values.
    pub fn add_array_u64(&mut self, name: &str, values: &[u64]) -> Result<()> {
        self.add_typed_array(name, DataType::ArrayUint64, values)
    }

    /// Adds an array of f32 values.
    pub fn add_array_f32(&mut self, name: &str, values: &[f32]) -> Result<()> {
        self.add_typed_array(name, DataType::ArrayFloat, values)
    }

    /// Adds an array of f64 values.
    pub fn add_array_f64(&mut self, name: &str, values: &[f64]) -> Result<()> {
        self.add_typed_array(name, DataType::ArrayDouble, values)
    }

    /// Adds an array of boolean values.
    pub fn add_array_bool(&mut self, name: &str, values: &[bool]) -> Result<()> {
        self.add_typed_array(name, DataType::ArrayBool, values)
    }

    /// Sets a field to null.
    pub fn set_field_null(&mut self, name: &str) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe { zvec_rust_sys::zvec_doc_set_field_null(self.handle, c_name.as_ptr()) })
    }

    /// Removes a field from the document.
    pub fn remove_field(&mut self, name: &str) -> Result<()> {
        let c_name = to_cstring(name)?;
        check_error(unsafe { zvec_rust_sys::zvec_doc_remove_field(self.handle, c_name.as_ptr()) })
    }

    // =========================================================================
    // Field getters
    // =========================================================================

    fn get_basic_field<T: Copy + Default>(
        &self,
        name: &str,
        data_type: DataType,
    ) -> Result<Option<T>> {
        if !self.has_field(name) || self.is_field_null(name) {
            return Ok(None);
        }
        let c_name = to_cstring(name)?;
        let mut value: T = T::default();
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_get_field_value_basic(
                self.handle,
                c_name.as_ptr(),
                data_type as u32,
                &mut value as *mut T as *mut c_void,
                std::mem::size_of::<T>(),
            )
        })?;
        Ok(Some(value))
    }

    fn get_pointer_field(
        &self,
        name: &str,
        data_type: DataType,
    ) -> Result<Option<(*const c_void, usize)>> {
        let c_name = to_cstring(name)?;
        let mut value_ptr: *const c_void = std::ptr::null();
        let mut value_size: usize = 0;
        check_error(unsafe {
            zvec_rust_sys::zvec_doc_get_field_value_pointer(
                self.handle,
                c_name.as_ptr(),
                data_type as u32,
                &mut value_ptr,
                &mut value_size,
            )
        })?;
        if value_ptr.is_null() || value_size == 0 {
            return Ok(None);
        }
        Ok(Some((value_ptr, value_size)))
    }

    fn get_typed_vec<T: Copy>(&self, name: &str, data_type: DataType) -> Result<Option<Vec<T>>> {
        let Some((ptr, size)) = self.get_pointer_field(name, data_type)? else {
            return Ok(None);
        };
        let elem_size = std::mem::size_of::<T>();
        if elem_size > 1 && size % elem_size != 0 {
            return Err(Error {
                code: ErrorCode::InternalError,
                message: format!(
                    "data size {} is not aligned to element size {}",
                    size, elem_size
                ),
            });
        }
        let count = size / elem_size;
        let slice = unsafe { std::slice::from_raw_parts(ptr as *const T, count) };
        Ok(Some(slice.to_vec()))
    }

    /// Gets a string field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_string(&self, name: &str) -> Result<Option<String>> {
        let Some((ptr, _size)) = self.get_pointer_field(name, DataType::String)? else {
            return Ok(None);
        };
        unsafe {
            let cstr = CStr::from_ptr(ptr as *const std::os::raw::c_char);
            Ok(Some(cstr.to_string_lossy().into_owned()))
        }
    }

    /// Gets a boolean field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_bool(&self, name: &str) -> Result<Option<bool>> {
        self.get_basic_field(name, DataType::Bool)
    }

    /// Gets an i32 field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_i32(&self, name: &str) -> Result<Option<i32>> {
        self.get_basic_field(name, DataType::Int32)
    }

    /// Gets an i64 field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_i64(&self, name: &str) -> Result<Option<i64>> {
        self.get_basic_field(name, DataType::Int64)
    }

    /// Gets a u32 field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_u32(&self, name: &str) -> Result<Option<u32>> {
        self.get_basic_field(name, DataType::Uint32)
    }

    /// Gets a u64 field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_u64(&self, name: &str) -> Result<Option<u64>> {
        self.get_basic_field(name, DataType::Uint64)
    }

    /// Gets an f32 field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_f32(&self, name: &str) -> Result<Option<f32>> {
        self.get_basic_field(name, DataType::Float)
    }

    /// Gets an f64 field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_f64(&self, name: &str) -> Result<Option<f64>> {
        self.get_basic_field(name, DataType::Double)
    }

    /// Gets a dense FP32 vector field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_vector_f32(&self, name: &str) -> Result<Option<Vec<f32>>> {
        self.get_typed_vec(name, DataType::VectorFp32)
    }

    /// Gets a dense FP64 vector field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_vector_f64(&self, name: &str) -> Result<Option<Vec<f64>>> {
        self.get_typed_vec(name, DataType::VectorFp64)
    }

    /// Gets a binary (raw bytes) field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_binary(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.get_typed_vec(name, DataType::Binary)
    }

    /// Gets a dense INT8 vector field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_vector_i8(&self, name: &str) -> Result<Option<Vec<i8>>> {
        self.get_typed_vec(name, DataType::VectorInt8)
    }

    /// Gets a dense INT16 vector field value. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_vector_i16(&self, name: &str) -> Result<Option<Vec<i16>>> {
        self.get_typed_vec(name, DataType::VectorInt16)
    }

    // =========================================================================
    // Array field getters
    // =========================================================================

    /// Gets an array of i32 values. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_array_i32(&self, name: &str) -> Result<Option<Vec<i32>>> {
        self.get_typed_vec(name, DataType::ArrayInt32)
    }

    /// Gets an array of i64 values. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_array_i64(&self, name: &str) -> Result<Option<Vec<i64>>> {
        self.get_typed_vec(name, DataType::ArrayInt64)
    }

    /// Gets an array of u32 values. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_array_u32(&self, name: &str) -> Result<Option<Vec<u32>>> {
        self.get_typed_vec(name, DataType::ArrayUint32)
    }

    /// Gets an array of u64 values. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_array_u64(&self, name: &str) -> Result<Option<Vec<u64>>> {
        self.get_typed_vec(name, DataType::ArrayUint64)
    }

    /// Gets an array of f32 values. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_array_f32(&self, name: &str) -> Result<Option<Vec<f32>>> {
        self.get_typed_vec(name, DataType::ArrayFloat)
    }

    /// Gets an array of f64 values. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_array_f64(&self, name: &str) -> Result<Option<Vec<f64>>> {
        self.get_typed_vec(name, DataType::ArrayDouble)
    }

    /// Gets an array of boolean values. Returns `Ok(None)` if the field does not exist or is null.
    pub fn get_array_bool(&self, name: &str) -> Result<Option<Vec<bool>>> {
        self.get_typed_vec(name, DataType::ArrayBool)
    }

    /// Clears all fields from the document.
    pub fn clear(&mut self) {
        unsafe { zvec_rust_sys::zvec_doc_clear(self.handle) };
    }
}

impl Drop for Doc {
    fn drop(&mut self) {
        if self.owned && !self.handle.is_null() {
            unsafe { zvec_rust_sys::zvec_doc_destroy(self.handle) };
        }
    }
}

/// Frees a vector of documents returned by query/fetch operations.
pub fn free_docs(docs: Vec<Doc>) {
    // Documents are freed individually via their Drop implementations
    drop(docs);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_borrowed_does_not_own() {
        // A borrowed doc should not destroy the handle on drop
        let doc = Doc::from_borrowed(std::ptr::null_mut());
        assert!(!doc.owned);
        assert!(doc.handle.is_null());
    }

    #[test]
    fn from_raw_takes_ownership() {
        let doc = unsafe { Doc::from_raw(std::ptr::null_mut()) };
        assert!(doc.owned);
        // Drop with null handle is safe (no-op)
    }
}
