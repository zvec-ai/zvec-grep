//! Index-local lookup cache derived from source records, never a separate database.
//!
//! Reservations are ephemeral until a `FileRecord` enters the existing write journal.
//! Deleted IDs need not survive reopening; IDs have no cross-generation contract.
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use crate::{
    EngineError, EngineResult,
    domain::{FileId, FileRecord, SourcePath},
};

#[derive(Default)]
pub(super) struct FileIds {
    by_path: HashMap<SourcePath, FileId>,
    by_id: HashMap<FileId, SourcePath>,
    next: Option<u32>,
    non_unicode_names: usize,
}

impl FileIds {
    pub(super) fn from_paths(
        paths: impl IntoIterator<Item = (FileId, PathBuf)>,
    ) -> EngineResult<Self> {
        let mut ids = Self {
            next: Some(0),
            ..Self::default()
        };
        for (id, path) in paths {
            ids.claim(id, SourcePath::new(path)?)?;
        }
        Ok(ids)
    }

    /// Validate a whole recovery batch before replaying any native mutations.
    pub(super) fn claim(&mut self, id: FileId, path: SourcePath) -> EngineResult<()> {
        if self
            .by_path
            .get(&path)
            .is_some_and(|existing| *existing != id)
            || self
                .by_id
                .get(&id)
                .is_some_and(|existing| *existing != path)
        {
            return Err(EngineError::storage_failure(
                "conflicting file IDs or paths in index records",
            ));
        }
        if !self.by_id.contains_key(&id) {
            self.non_unicode_names += usize::from(non_unicode_name(&path));
            self.by_path.insert(path.clone(), id);
            self.by_id.insert(id, path);
        }
        if self.next.is_some_and(|next| id.get() >= next) {
            self.next = id.get().checked_add(1);
        }
        Ok(())
    }

    pub(super) fn resolve(&mut self, paths: &[PathBuf]) -> EngineResult<Vec<FileId>> {
        let paths = paths
            .iter()
            .map(SourcePath::new)
            .collect::<EngineResult<Vec<_>>>()?;
        let missing = paths
            .iter()
            .filter(|path| !self.by_path.contains_key(*path))
            .collect::<HashSet<_>>()
            .len();
        if missing > 0 {
            let next = self.next.ok_or_else(exhausted)?;
            if u128::from(next) + missing as u128 > u128::from(u32::MAX) + 1 {
                return Err(exhausted());
            }
        }
        let mut result = Vec::with_capacity(paths.len());
        for path in paths {
            let id = if let Some(id) = self.by_path.get(&path) {
                *id
            } else {
                let id = FileId::new(self.next.ok_or_else(exhausted)?);
                self.claim(id, path)?;
                id
            };
            result.push(id);
        }
        Ok(result)
    }

    pub(super) fn validate(&self, file: &FileRecord) -> EngineResult<()> {
        file.validate()?;
        if self.by_path.get(&file.relative_path) != Some(&file.id) {
            return Err(EngineError::invalid_argument(
                "file ID does not match its index-local path",
            ));
        }
        Ok(())
    }

    pub(super) fn remove(&mut self, id: FileId) {
        if let Some(path) = self.by_id.remove(&id) {
            self.by_path.remove(&path);
            self.non_unicode_names -= usize::from(non_unicode_name(&path));
        }
    }

    pub(super) fn has_non_unicode_file_names(&self) -> bool {
        self.non_unicode_names != 0
    }
}

fn non_unicode_name(path: &SourcePath) -> bool {
    path.file_name().is_some_and(|name| name.to_str().is_none())
}

fn exhausted() -> EngineError {
    EngineError::storage_failure("index file ID allocation range is exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuilds_from_records_and_resolves_duplicates_without_io() {
        let mut ids =
            FileIds::from_paths([(FileId::new(9), PathBuf::from("src/old.rs"))]).expect("records");
        let paths = ["src/old.rs", "new.rs", "new.rs"].map(PathBuf::from);
        assert_eq!(
            ids.resolve(&paths).expect("IDs"),
            [FileId::new(9), FileId::new(10), FileId::new(10)]
        );
        assert_eq!(
            ids.resolve(&[PathBuf::from("other.rs")]).expect("ID"),
            [FileId::new(11)]
        );
    }

    #[test]
    fn rejects_invalid_paths_and_exhaustion_without_partial_allocation() {
        let mut ids = FileIds::from_paths([(FileId::new(u32::MAX - 1), PathBuf::from("old"))])
            .expect("records");
        assert!(ids.resolve(&["a", "../bad"].map(PathBuf::from)).is_err());
        assert!(ids.resolve(&["a", "b"].map(PathBuf::from)).is_err());
        assert_eq!(
            ids.resolve(&["a", "a"].map(PathBuf::from)).expect("last"),
            [FileId::new(u32::MAX); 2]
        );
        assert!(ids.resolve(&[PathBuf::from("b")]).is_err());
        assert_eq!(
            ids.resolve(&[PathBuf::from("old")]).expect("existing"),
            [FileId::new(u32::MAX - 1)]
        );
    }

    #[test]
    fn rejects_conflicting_records() {
        for records in [
            [
                (FileId::new(1), PathBuf::from("a")),
                (FileId::new(1), PathBuf::from("b")),
            ],
            [
                (FileId::new(1), PathBuf::from("a")),
                (FileId::new(2), PathBuf::from("a")),
            ],
        ] {
            assert!(FileIds::from_paths(records).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_paths_remain_distinct_and_deletion_clears_fallback() {
        use std::os::unix::ffi::OsStringExt;
        let paths =
            [0xfe, 0xff].map(|byte| PathBuf::from(std::ffi::OsString::from_vec(vec![byte])));
        let mut ids = FileIds::from_paths([]).expect("empty");
        let allocated = ids.resolve(&paths).expect("native IDs");
        assert_ne!(allocated[0], allocated[1]);
        assert!(ids.has_non_unicode_file_names());
        for id in allocated {
            ids.remove(id);
        }
        assert!(!ids.has_non_unicode_file_names());
    }
}
