//! Compact directory IDs derived from source membership, scoped to one index.
//! Native directory records restore this lookup when it is first needed.
use std::collections::HashMap;

use crate::{
    EngineError, EngineResult,
    domain::{DirectoryId, DirectoryRecord, SourcePath},
};

pub(super) struct DirectoryIds {
    by_path: HashMap<SourcePath, DirectoryId>,
    by_id: HashMap<DirectoryId, SourcePath>,
    next: Option<u32>,
}

impl Default for DirectoryIds {
    fn default() -> Self {
        Self {
            by_path: HashMap::new(),
            by_id: HashMap::new(),
            next: Some(0),
        }
    }
}

impl DirectoryIds {
    pub(super) fn claim(&mut self, directory: DirectoryRecord) -> EngineResult<()> {
        let DirectoryRecord {
            id,
            relative_path: path,
        } = directory;
        if self.by_path.get(&path).is_some_and(|other| *other != id)
            || self.by_id.get(&id).is_some_and(|other| *other != path)
        {
            return Err(invalid("conflicting directory IDs in source records"));
        }
        if !self.by_id.contains_key(&id) {
            self.by_path.insert(path.clone(), id);
            self.by_id.insert(id, path);
        }
        if self.next.is_some_and(|next| id.get() >= next) {
            self.next = id.get().checked_add(1);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn add_source(&mut self, file: &SourcePath, ids: &[u32]) -> EngineResult<()> {
        let ancestors = ancestors(file)?;
        if ancestors.len() != ids.len() {
            return Err(invalid(
                "source directory membership does not match path depth",
            ));
        }
        for (relative_path, id) in ancestors.into_iter().zip(ids) {
            self.claim(DirectoryRecord {
                id: DirectoryId::new(*id),
                relative_path,
            })?;
        }
        Ok(())
    }

    pub(super) fn resolve(&mut self, file: &SourcePath) -> EngineResult<Vec<DirectoryId>> {
        let ancestors = ancestors(file)?;
        let missing = ancestors
            .iter()
            .filter(|path| !self.by_path.contains_key(*path))
            .count();
        if missing > 0 {
            let next = self
                .next
                .ok_or_else(|| invalid("directory ID range exhausted"))?;
            if u128::from(next) + missing as u128 > u128::from(u32::MAX) + 1 {
                return Err(invalid("directory ID range exhausted"));
            }
        }
        ancestors
            .into_iter()
            .map(|path| {
                if let Some(id) = self.by_path.get(&path) {
                    return Ok(*id);
                }
                let id = DirectoryId::new(
                    self.next
                        .ok_or_else(|| invalid("directory ID range exhausted"))?,
                );
                self.claim(DirectoryRecord {
                    id,
                    relative_path: path,
                })?;
                Ok(id)
            })
            .collect()
    }

    pub(super) fn get(&self, path: &SourcePath) -> Option<DirectoryId> {
        self.by_path.get(path).copied()
    }
}

fn ancestors(path: &SourcePath) -> EngineResult<Vec<SourcePath>> {
    let mut paths = path
        .ancestors()
        .skip(1)
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(SourcePath::new)
        .collect::<EngineResult<Vec<_>>>()?;
    paths.reverse();
    Ok(paths)
}

fn invalid(message: impl Into<String>) -> EngineError {
    EngineError::storage_failure(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstructs_compact_membership_and_rejects_conflicts() {
        let mut ids = DirectoryIds::default();
        let path = SourcePath::new("src/deep/file.rs").expect("path");
        ids.add_source(&path, &[7, 9]).expect("source");
        assert_eq!(
            ids.resolve(&path).expect("IDs"),
            [DirectoryId::new(7), DirectoryId::new(9)]
        );
        let sibling = SourcePath::new("src/new/file.rs").expect("path");
        assert_eq!(
            ids.resolve(&sibling).expect("IDs"),
            [DirectoryId::new(7), DirectoryId::new(10)]
        );
        assert!(ids.add_source(&path, &[7]).is_err());
        assert!(ids.add_source(&path, &[8, 9]).is_err());
        assert!(
            ids.add_source(&SourcePath::new("other/file.rs").expect("path"), &[7])
                .is_err()
        );
    }

    #[test]
    fn u32_exhaustion_does_not_partially_allocate_ancestors() {
        let mut ids = DirectoryIds::default();
        ids.add_source(&SourcePath::new("old/file").expect("path"), &[u32::MAX - 1])
            .expect("source");
        let nested = SourcePath::new("new/nested/file").expect("path");
        assert!(ids.resolve(&nested).is_err());
        assert_eq!(ids.get(&SourcePath::new("new").expect("path")), None);
        let last = SourcePath::new("new/file").expect("path");
        assert_eq!(
            ids.resolve(&last).expect("last ID"),
            [DirectoryId::new(u32::MAX)]
        );
        assert_eq!(
            ids.resolve(&last).expect("existing ID"),
            [DirectoryId::new(u32::MAX)]
        );
        assert!(ids.resolve(&nested).is_err());
        assert!(
            ids.resolve(&SourcePath::new("root-file").expect("path"))
                .expect("no ancestors")
                .is_empty()
        );
    }
}
