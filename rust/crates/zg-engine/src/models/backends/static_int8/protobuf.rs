//! Bounded reader for the small ONNX protobuf subset used by pinned static graphs.

use crate::models::spi::ModelError;

#[derive(Clone, Copy)]
pub(super) enum Value<'a> {
    Integer(u64),
    Bytes(&'a [u8]),
    Fixed,
}

impl<'a> Value<'a> {
    pub(super) fn bytes(self) -> Result<&'a [u8], ModelError> {
        match self {
            Self::Bytes(bytes) => Ok(bytes),
            _ => Err(invalid()),
        }
    }

    pub(super) fn integer(self) -> Result<u64, ModelError> {
        match self {
            Self::Integer(value) => Ok(value),
            _ => Err(invalid()),
        }
    }

    pub(super) fn string(self) -> Result<&'a str, ModelError> {
        std::str::from_utf8(self.bytes()?).map_err(|_| invalid())
    }
}

pub(super) struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(super) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    pub(super) fn varint(&mut self) -> Result<u64, ModelError> {
        let mut value = 0_u64;
        for shift in (0..70).step_by(7) {
            let byte = self.take(1)?[0];
            if shift == 63 && byte > 1 {
                return Err(invalid());
            }
            value |= u64::from(byte & 127) << shift;
            if byte < 128 {
                return Ok(value);
            }
        }
        Err(invalid())
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ModelError> {
        let (value, rest) = self.bytes.split_at_checked(count).ok_or_else(invalid)?;
        self.bytes = rest;
        Ok(value)
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub(super) fn next(&mut self) -> Result<Option<(u32, Value<'a>)>, ModelError> {
        if self.is_empty() {
            return Ok(None);
        }
        let key = self.varint()?;
        let tag = u32::try_from(key >> 3).map_err(|_| invalid())?;
        if tag == 0 || tag >= (1 << 29) {
            return Err(invalid());
        }
        let value = match key & 7 {
            0 => Value::Integer(self.varint()?),
            2 => {
                let length = usize::try_from(self.varint()?).map_err(|_| invalid())?;
                Value::Bytes(self.take(length)?)
            }
            1 => {
                self.take(8)?;
                Value::Fixed
            }
            5 => {
                self.take(4)?;
                Value::Fixed
            }
            _ => return Err(invalid()),
        };
        Ok(Some((tag, value)))
    }
}

pub(super) fn invalid() -> ModelError {
    ModelError::storage_failure("Invalid or unsupported static INT8 ONNX graph")
}

#[cfg(test)]
#[path = "protobuf/tests.rs"]
mod tests;
