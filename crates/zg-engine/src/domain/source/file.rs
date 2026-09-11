use std::path::{Component, Path, PathBuf};

use crate::{EngineError, EngineResult};

use super::format::{FileCategory, FileFormat};

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

/// Describes the original file and its last observed state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceFile {
    pub id: FileId,
    pub absolute_path: PathBuf,
    pub relative_path: PathBuf,
    pub root_path: PathBuf,
    pub formats: Vec<FileFormat>,
    pub snapshot: FileSnapshot,
}

impl SourceFile {
    pub(crate) fn has_category(&self, category: FileCategory) -> bool {
        self.formats
            .iter()
            .any(|format| format.categories().contains(&category))
    }

    #[track_caller]
    pub(crate) fn validate(&self) -> EngineResult<()> {
        if !self.absolute_path.is_absolute() || !self.root_path.is_absolute() {
            return Err(EngineError::invalid_argument(
                "source file and root paths must be absolute",
            ));
        }
        if self.relative_path.as_os_str().is_empty()
            || self
                .relative_path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(EngineError::invalid_argument(
                "source relative path must stay within its root",
            ));
        }
        let expected = if self.root_path == self.absolute_path {
            self.root_path.file_name().map(Path::new)
        } else {
            self.absolute_path.strip_prefix(&self.root_path).ok()
        };
        if expected != Some(self.relative_path.as_path()) {
            return Err(EngineError::invalid_argument(
                "source paths do not identify the same file",
            ));
        }
        if self.formats.is_empty()
            || (self.formats.len() > 1 && self.formats.contains(&FileFormat::Unknown))
            || self
                .formats
                .iter()
                .enumerate()
                .any(|(index, format)| self.formats[..index].contains(format))
        {
            return Err(EngineError::invalid_argument(
                "source formats must be non-empty and unique; unknown must stand alone",
            ));
        }
        Ok(())
    }
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

    #[test]
    fn validates_paths_and_format_sets_without_reading_the_file() {
        let root = std::env::current_dir().expect("current directory");
        let file = SourceFile {
            id: FileId::new("file").expect("id"),
            absolute_path: root.join("nested/fixture.rs"),
            relative_path: PathBuf::from("nested/fixture.rs"),
            root_path: root,
            formats: vec![FileFormat::Rust],
            snapshot: FileSnapshot {
                size_bytes: 0,
                modified_epoch_ms: None,
                content_hash: None,
            },
        };
        file.validate().expect("valid source");
        assert!(file.has_category(FileCategory::Code));
        assert!(!file.has_category(FileCategory::Binary));
        let mut root_file = file.clone();
        root_file.root_path = file.absolute_path.clone();
        root_file.relative_path = PathBuf::from("fixture.rs");
        root_file.validate().expect("root may be a file");
        for path in ["", "../fixture.rs", "other.rs"] {
            let mut invalid = file.clone();
            invalid.relative_path = PathBuf::from(path);
            assert!(invalid.validate().is_err(), "{path}");
        }
        for formats in [
            vec![],
            vec![FileFormat::Rust, FileFormat::Rust],
            vec![FileFormat::Rust, FileFormat::Unknown],
        ] {
            let mut invalid = file.clone();
            invalid.formats = formats;
            assert!(invalid.validate().is_err());
        }
        let mut unknown = file;
        unknown.formats = vec![FileFormat::Unknown];
        unknown
            .validate()
            .expect("unknown is a valid classification");
    }
}
