use std::path::{Component, Path, PathBuf};

use crate::{EngineError, EngineResult};

/// A validated native path relative to a workspace root.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SourcePath(PathBuf);

impl SourcePath {
    #[track_caller]
    pub(crate) fn new(path: impl Into<PathBuf>) -> EngineResult<Self> {
        let path = path.into();
        Self::validate(&path)?;
        Ok(Self(path))
    }

    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn into_path_buf(self) -> PathBuf {
        self.0
    }

    fn validate(path: &Path) -> EngineResult<()> {
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            || path.as_os_str().as_encoded_bytes().contains(&0)
        {
            return Err(EngineError::invalid_argument(
                "source path must be a relative file or directory path within the workspace",
            ));
        }
        let normalized: PathBuf = path.components().collect();
        #[cfg(windows)]
        let same_spelling = normalized
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .map(|byte| if *byte == b'/' { b'\\' } else { *byte })
            .eq(path
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .map(|byte| if *byte == b'/' { b'\\' } else { *byte }));
        #[cfg(not(windows))]
        let same_spelling = normalized.as_os_str() == path.as_os_str();
        if !same_spelling {
            return Err(EngineError::invalid_argument(
                "source path must use normalized native path components",
            ));
        }
        Ok(())
    }
}

impl AsRef<Path> for SourcePath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

// Only immutable Path operations are exposed; mutation requires constructing
// another SourcePath and validating it again.
impl std::ops::Deref for SourcePath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.as_path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_common_paths() {
        for path in [
            "file.rs",
            "src/file.rs",
            "with space / file.rs ",
            "src/.hidden",
            "中文/文件",
            "한국어/日本語.txt",
        ] {
            let source = SourcePath::new(path).expect("native relative path");
            assert_eq!(source.as_path(), Path::new(path));
            assert_eq!(source.into_path_buf(), PathBuf::from(path));
        }
    }

    #[cfg(unix)]
    #[test]
    fn creates_unix_paths() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        for bytes in [
            b"src\\name/file.rs".as_slice(),
            b"src/invalid-\xff",
            b"src/\\..\\file",
        ] {
            let path = Path::new(OsStr::from_bytes(bytes));
            let path = SourcePath::new(path).expect("native Unix path");
            assert_eq!(path.as_os_str().as_bytes(), bytes);
        }
    }

    #[cfg(windows)]
    #[test]
    fn creates_windows_paths() {
        use std::{
            ffi::OsString,
            os::windows::ffi::{OsStrExt, OsStringExt},
        };

        for path in [
            r"src\engine\file.rs",
            "src/engine/file.rs",
            r"src\engine/file.rs",
        ] {
            SourcePath::new(path).expect("native Windows separators");
        }
        let units = [b's' as u16, b'/' as u16, 0xd800];
        let path = PathBuf::from(OsString::from_wide(&units));
        let path = SourcePath::new(path).expect("native Windows path");
        assert_eq!(path.as_os_str().encode_wide().collect::<Vec<_>>(), units);
    }

    #[test]
    fn rejects_invalid_paths() {
        for path in [
            "",
            ".",
            "..",
            "/",
            "/src/file.rs",
            "./src/file.rs",
            "../file.rs",
            "src/../file.rs",
            "src/./file.rs",
            "src//file.rs",
            "src/file.rs/",
            "src/file.rs/.",
            "src/bad\0name",
        ] {
            assert!(SourcePath::new(path).is_err(), "{path:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn rejects_invalid_windows_paths() {
        for path in [
            r"C:\src\file.rs",
            r"C:src\file.rs",
            r"\src\file.rs",
            r"\\server\share\file.rs",
            r"src\\file.rs",
            r"src\.\file.rs",
            r"src\/file.rs",
        ] {
            assert!(SourcePath::new(path).is_err(), "{path:?}");
        }
    }
}
