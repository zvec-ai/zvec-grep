//! Live path mappings and durable allocation within one storage generation.
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    EngineError, EngineResult,
    domain::{FileId, FileRecord, SourcePath},
    utils::atomic_write,
};

const CAPACITY: u64 = u32::MAX as u64 + 1;

pub(super) struct FileIds {
    by_path: HashMap<SourcePath, FileId>,
    by_id: HashMap<FileId, SourcePath>,
    sequence_path: PathBuf,
    next: u64,
    non_unicode_names: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Sequence {
    next: u64,
}

impl FileIds {
    /// Initialize allocation before creating the collection, or load its durable sequence.
    pub(super) fn open(root: &Path) -> EngineResult<Self> {
        let sequence_path = root.join("file-ids.json");
        let next = match fs::read(&sequence_path) {
            Ok(bytes) => {
                let sequence: Sequence = serde_json::from_slice(&bytes).map_err(|error| {
                    invalid_sequence(&sequence_path, &format!("invalid JSON: {error}"))
                })?;
                if sequence.next > CAPACITY {
                    return Err(invalid_sequence(
                        &sequence_path,
                        "allocation range exceeded",
                    ));
                }
                sequence.next
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let collection = root.join("files");
                if collection.try_exists().map_err(|error| {
                    EngineError::from_io(
                        format!(
                            "cannot inspect file collection {}: {error}",
                            collection.display()
                        ),
                        &error,
                    )
                })? {
                    return Err(invalid_sequence(
                        &sequence_path,
                        "missing allocation sequence",
                    ));
                }
                write_sequence(&sequence_path, 0)?;
                0
            }
            Err(error) => {
                return Err(EngineError::from_io(
                    format!(
                        "cannot read file ID sequence {}: {error}",
                        sequence_path.display()
                    ),
                    &error,
                ));
            }
        };
        Ok(Self {
            by_path: HashMap::new(),
            by_id: HashMap::new(),
            sequence_path,
            next,
            non_unicode_names: 0,
        })
    }

    pub(super) fn restore(
        &mut self,
        paths: impl IntoIterator<Item = (FileId, PathBuf)>,
    ) -> EngineResult<()> {
        for (id, path) in paths {
            if u64::from(id.get()) >= self.next {
                return Err(invalid_sequence(
                    &self.sequence_path,
                    "stored file ID exceeds the reserved range",
                ));
            }
            self.claim(id, SourcePath::new(path)?)?;
        }
        Ok(())
    }

    /// Restore a stored identity or reserve a newly allocated one.
    fn claim(&mut self, id: FileId, path: SourcePath) -> EngineResult<()> {
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
        let mut next = self.next;
        let reserved = next
            .checked_add(u64::try_from(missing).map_err(|_| exhausted())?)
            .filter(|value| *value <= CAPACITY)
            .ok_or_else(exhausted)?;
        if missing != 0 {
            // Publish the high-water mark before exposing any of these IDs. An
            // interrupted reservation may leave gaps, but cannot reuse an ID.
            write_sequence(&self.sequence_path, reserved)?;
            self.next = reserved;
        }
        let mut result = Vec::with_capacity(paths.len());
        for path in paths {
            let id = if let Some(id) = self.by_path.get(&path) {
                *id
            } else {
                let id = FileId::new(u32::try_from(next).map_err(|_| exhausted())?);
                next += 1;
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

fn write_sequence(path: &Path, next: u64) -> EngineResult<()> {
    let bytes = serde_json::to_vec(&Sequence { next }).map_err(|error| {
        EngineError::storage_failure(format!("cannot encode file ID sequence: {error}"))
    })?;
    atomic_write(path, &bytes)
}

fn invalid_sequence(path: &Path, detail: &str) -> EngineError {
    EngineError::storage_failure(format!(
        "invalid file ID sequence {}: {detail}; rebuild the index",
        path.display()
    ))
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

    fn from_paths(
        paths: impl IntoIterator<Item = (FileId, PathBuf)>,
    ) -> EngineResult<(tempfile::TempDir, FileIds)> {
        let root = tempfile::tempdir().expect("allocation directory");
        let paths = paths.into_iter().collect::<Vec<_>>();
        let next = paths
            .iter()
            .map(|(id, _)| u64::from(id.get()) + 1)
            .max()
            .unwrap_or(0);
        write_sequence(&root.path().join("file-ids.json"), next)?;
        let mut ids = FileIds::open(root.path())?;
        ids.restore(paths)?;
        Ok((root, ids))
    }

    #[test]
    fn restores_records_and_resolves_duplicates() {
        let (_root, mut ids) =
            from_paths([(FileId::new(9), PathBuf::from("src/old.rs"))]).expect("records");
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
        let (root, mut ids) =
            from_paths([(FileId::new(u32::MAX - 1), PathBuf::from("old"))]).expect("records");
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
        let mut reopened = FileIds::open(root.path()).expect("reopen exhausted allocator");
        assert!(reopened.resolve(&[PathBuf::from("new")]).is_err());
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
            assert!(from_paths(records).is_err());
        }
    }

    #[test]
    fn reservations_survive_reopen_without_any_stored_records() {
        let (root, mut ids) = from_paths([]).expect("allocator");
        let first = ids.resolve(&[PathBuf::from("old")]).expect("first ID")[0];
        drop(ids);
        let mut reopened = FileIds::open(root.path()).expect("reopen empty collection");
        let second = reopened.resolve(&[PathBuf::from("new")]).expect("new ID")[0];
        assert!(second > first);
    }

    #[test]
    fn failed_persistence_does_not_expose_an_unreserved_id() {
        let (root, mut ids) = from_paths([]).expect("allocator");
        fs::remove_file(&ids.sequence_path).expect("remove sequence");
        fs::create_dir(&ids.sequence_path).expect("block sequence publication");
        assert!(ids.resolve(&[PathBuf::from("new")]).is_err());
        assert!(ids.by_path.is_empty());
        assert_eq!(ids.next, 0);
        fs::remove_dir(&ids.sequence_path).expect("unblock sequence publication");
        let id = ids.resolve(&[PathBuf::from("new")]).expect("reserve ID")[0];
        assert_eq!(id, FileId::new(0));
        let mut reopened = FileIds::open(root.path()).expect("reopen durable sequence");
        assert_eq!(
            reopened
                .resolve(&[PathBuf::from("other")])
                .expect("next ID"),
            [FileId::new(1)]
        );
    }

    #[test]
    fn corrupt_or_missing_sequences_are_rejected_without_resetting_allocation() {
        let root = tempfile::tempdir().expect("allocation directory");
        fs::create_dir(root.path().join("files")).expect("existing collection");
        assert!(FileIds::open(root.path()).is_err());
        let path = root.path().join("file-ids.json");
        assert!(!path.exists());
        for bytes in [
            b"not JSON".as_slice(),
            b"{}",
            b"{\"next\":null}",
            b"{\"next\":-1}",
            b"{\"next\":4294967297}",
            b"{\"next\":2,\"unknown\":true}",
        ] {
            fs::write(&path, bytes).expect("corrupt sequence");
            assert!(FileIds::open(root.path()).is_err());
            assert_eq!(fs::read(&path).expect("unchanged sequence"), bytes);
        }
        write_sequence(&path, 2).expect("sequence");
        let mut ids = FileIds::open(root.path()).expect("valid sequence");
        assert!(
            ids.restore([(FileId::new(2), PathBuf::from("out-of-range"))])
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_paths_remain_distinct_and_deletion_clears_fallback() {
        use std::os::unix::ffi::OsStringExt;
        let paths =
            [0xfe, 0xff].map(|byte| PathBuf::from(std::ffi::OsString::from_vec(vec![byte])));
        let (_root, mut ids) = from_paths([]).expect("empty");
        let allocated = ids.resolve(&paths).expect("native IDs");
        assert_ne!(allocated[0], allocated[1]);
        assert!(ids.has_non_unicode_file_names());
        for id in allocated {
            ids.remove(id);
        }
        assert!(!ids.has_non_unicode_file_names());
    }
}
