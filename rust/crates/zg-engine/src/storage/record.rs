//! Versioned JSON records shared by the physical storage tables.
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{EngineError, EngineResult};

pub(super) const VERSION: u16 = 12;

pub(super) fn encode(value: impl Serialize, kind: &str) -> EngineResult<String> {
    serde_json::to_string(&Record {
        version: VERSION,
        value,
    })
    .map_err(|error| EngineError::storage_failure(format!("failed to encode {kind}: {error}")))
}

pub(super) fn decode<T: DeserializeOwned>(json: &str, kind: &str) -> EngineResult<T> {
    let decode_error =
        |error| EngineError::storage_failure(format!("failed to decode stored {kind}: {error}"));
    let record: Record<&serde_json::value::RawValue> =
        serde_json::from_str(json).map_err(decode_error)?;
    if record.version != VERSION {
        return Err(EngineError::storage_failure(format!(
            "unsupported stored {kind} version {}; expected {VERSION}; rebuild the index",
            record.version
        )));
    }
    serde_json::from_str(record.value.get()).map_err(decode_error)
}

pub(super) fn invalid_record(kind: &str, error: &EngineError) -> EngineError {
    EngineError::storage_failure(format!("invalid stored {kind}: {}", error.message()))
}

#[derive(Serialize, Deserialize)]
struct Record<T> {
    version: u16,
    value: T,
}
