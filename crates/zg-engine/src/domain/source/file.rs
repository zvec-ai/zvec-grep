use std::path::PathBuf;

use crate::{EngineError, EngineResult};

use super::format::FileFormat;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct FileId(String);

impl FileId {
    #[track_caller]
    pub(crate) fn new(value: impl Into<String>) -> EngineResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(EngineError::invalid_argument("file id must not be blank"));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileSnapshot {
    pub size_bytes: u64,
    pub modified_epoch_ms: Option<u64>,
    pub content_hash: Option<String>,
}

/// Describes the original file; extracted payloads live in content values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceFile {
    pub id: FileId,
    pub absolute_path: PathBuf,
    pub relative_path: PathBuf,
    pub root_path: PathBuf,
    pub formats: Vec<FileFormat>,
    pub snapshot: FileSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_id_rejects_blank_inputs_without_normalizing() {
        for blank in ["", " ", "\t\n", "\u{2003}"] {
            assert!(FileId::new(blank).is_err());
        }
        assert_eq!(
            FileId::new(" file-1 ").expect("file id").as_str(),
            " file-1 "
        );
    }
}
