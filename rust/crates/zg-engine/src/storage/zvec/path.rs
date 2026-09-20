//! Lossless identity keys and optional Unicode query projections are separate.

use std::path::{Path, PathBuf};

use crate::{EngineError, EngineResult, domain::SourcePath};

use super::codec::PathRecord;

pub(crate) fn encode_path(path: &SourcePath) -> EngineResult<String> {
    // Component collection unifies accepted Windows separator spellings while
    // leaving Unix backslashes and platform-native non-Unicode names intact.
    let canonical: PathBuf = path.components().collect();
    serde_json::to_string(&PathRecord::from_path(&canonical)?).map_err(|error| {
        EngineError::storage_failure(format!("cannot encode identity path: {error}"))
    })
}

pub(crate) fn decode_path(value: &str) -> EngineResult<SourcePath> {
    let record: PathRecord = serde_json::from_str(value).map_err(|error| {
        EngineError::storage_failure(format!("cannot decode identity path: {error}"))
    })?;
    SourcePath::new(record.into_path()?)
}

/// Hex preserves the full native representation and needs no SQL escaping.
pub(crate) fn path_key(path: &SourcePath) -> EngineResult<String> {
    Ok(hex::encode(encode_path(path)?.as_bytes()))
}

/// This projection accelerates path matching; it never establishes identity.
pub(crate) fn query_path(path: &Path) -> Option<String> {
    let value = path.to_str()?;
    #[cfg(windows)]
    return Some(value.replace('\\', "/"));
    #[cfg(not(windows))]
    Some(value.to_owned())
}
