//! JSON records governed by the workspace's physical index version.
use serde::{Serialize, de::DeserializeOwned};

use crate::{EngineError, EngineResult};

pub(super) fn encode(value: impl Serialize, kind: &str) -> EngineResult<String> {
    serde_json::to_string(&value)
        .map_err(|error| EngineError::storage_failure(format!("failed to encode {kind}: {error}")))
}

pub(super) fn decode<T: DeserializeOwned>(json: &str, kind: &str) -> EngineResult<T> {
    serde_json::from_str(json).map_err(|error| {
        EngineError::storage_failure(format!("failed to decode stored {kind}: {error}"))
    })
}

pub(super) fn invalid_record(kind: &str, error: &EngineError) -> EngineError {
    EngineError::storage_failure(format!("invalid stored {kind}: {}", error.message()))
}
