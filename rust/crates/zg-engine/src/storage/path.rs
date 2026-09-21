//! Lossless identity keys and optional Unicode query projections are separate.

use std::path::{Path, PathBuf};

use crate::{EngineError, EngineResult, domain::SourcePath};

use super::zvec::{native, scalar, wildcard_string};
use crate::domain::{DirectoryId, FileRecord};
use serde::{Deserialize, Serialize};
use zvec_rust::{CollectionSchema, DataType, Doc};

pub(super) fn encode_path(path: &SourcePath) -> EngineResult<String> {
    // Component collection unifies accepted Windows separator spellings while
    // leaving Unix backslashes and platform-native non-Unicode names intact.
    let canonical: PathBuf = path.components().collect();
    serde_json::to_string(&PathRecord::from_path(&canonical)?).map_err(|error| {
        EngineError::storage_failure(format!("cannot encode identity path: {error}"))
    })
}

pub(super) fn decode_path(value: &str) -> EngineResult<SourcePath> {
    let record: PathRecord = serde_json::from_str(value).map_err(|error| {
        EngineError::storage_failure(format!("cannot decode identity path: {error}"))
    })?;
    SourcePath::new(record.into_path()?)
}

/// Hex preserves the full native representation and needs no SQL escaping.
pub(super) fn path_key(path: &SourcePath) -> EngineResult<String> {
    Ok(hex::encode(encode_path(path)?.as_bytes()))
}

/// This projection accelerates path matching; it never establishes identity.
pub(super) fn query_path(path: &Path) -> Option<String> {
    let value = path.to_str()?;
    #[cfg(windows)]
    return Some(value.replace('\\', "/"));
    #[cfg(not(windows))]
    Some(value.to_owned())
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "encoding", content = "value", rename_all = "snake_case")]
pub(super) enum PathRecord {
    Utf8(String),
    UnixBytes(Vec<u8>),
    WindowsWide(Vec<u16>),
}

impl PathRecord {
    pub(super) fn from_path(path: &Path) -> EngineResult<Self> {
        validate_path(path)?;
        if let Some(value) = path.to_str() {
            return Ok(Self::Utf8(value.to_owned()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Ok(Self::UnixBytes(path.as_os_str().as_bytes().to_vec()))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Ok(Self::WindowsWide(path.as_os_str().encode_wide().collect()))
        }
        #[cfg(not(any(unix, windows)))]
        Err(EngineError::storage_failure(
            "cannot store a non-Unicode path on this platform",
        ))
    }

    pub(super) fn into_path(self) -> EngineResult<PathBuf> {
        match self {
            Self::Utf8(value) => Ok(PathBuf::from(value)),
            Self::UnixBytes(bytes) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt;
                    Ok(std::ffi::OsString::from_vec(bytes).into())
                }
                #[cfg(not(unix))]
                String::from_utf8(bytes).map(PathBuf::from).map_err(|_| {
                    EngineError::invalid_argument(
                        "stored Unix path cannot be represented on this platform",
                    )
                })
            }
            Self::WindowsWide(units) => {
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStringExt;
                    Ok(std::ffi::OsString::from_wide(&units).into())
                }
                #[cfg(not(windows))]
                String::from_utf16(&units).map(PathBuf::from).map_err(|_| {
                    EngineError::invalid_argument(
                        "stored Windows path cannot be represented on this platform",
                    )
                })
            }
        }
    }
}

fn validate_path(path: &Path) -> EngineResult<()> {
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(EngineError::invalid_argument(
            "source file path must not contain NUL",
        ));
    }
    Ok(())
}

pub(super) fn file_membership_schema(schema: &mut CollectionSchema) -> EngineResult<()> {
    scalar(
        schema,
        "ancestor_directory_ids",
        DataType::ArrayUint32,
        false,
        true,
    )?;
    wildcard_string(schema, "file_name", false)
}

pub(super) fn file_membership_doc(
    doc: &mut Doc,
    file: &FileRecord,
    directories: &[DirectoryId],
) -> EngineResult<()> {
    native(
        doc.add_array_u32(
            "ancestor_directory_ids",
            &directories.iter().map(|id| id.get()).collect::<Vec<_>>(),
        ),
        "encode ancestor directories",
    )?;
    // Non-Unicode names have no STRING representation. The source-path cache disables name
    // pushdown in this workspace; exact native paths remain in FileRecord.
    let name = file
        .relative_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    native(doc.add_string("file_name", name), "encode file name")?;
    Ok(())
}
