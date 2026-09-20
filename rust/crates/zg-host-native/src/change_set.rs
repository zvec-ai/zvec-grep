use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use crate::api::{WorkspaceChange, WorkspaceChangeBatch};

#[derive(Debug)]
pub(crate) struct ChangeSet {
    upserts: BTreeSet<PathBuf>,
    deletes: BTreeSet<PathBuf>,
    rescan_directories: BTreeSet<PathBuf>,
    deleted_prefixes: BTreeSet<PathBuf>,
    full_rescan: bool,
    max_changed_paths: usize,
}

impl ChangeSet {
    pub fn new(max_changed_paths: usize) -> Self {
        Self {
            upserts: BTreeSet::new(),
            deletes: BTreeSet::new(),
            rescan_directories: BTreeSet::new(),
            deleted_prefixes: BTreeSet::new(),
            full_rescan: false,
            max_changed_paths: max_changed_paths.max(1),
        }
    }

    pub fn add(&mut self, change: WorkspaceChange) {
        if self.full_rescan {
            return;
        }
        match change {
            WorkspaceChange::Rescan => self.full_rescan = true,
            WorkspaceChange::Upsert(path) => {
                self.remove_exact(&path);
                if !self.covered_by_ancestor(&path) {
                    self.upserts.insert(path);
                }
            }
            WorkspaceChange::Delete(path) => {
                self.remove_exact(&path);
                if !self.covered_by_ancestor(&path) {
                    self.deletes.insert(path);
                }
            }
            WorkspaceChange::RescanDirectory(path) => {
                self.remove_exact(&path);
                if !self.covered_by_ancestor(&path) {
                    self.remove_descendants(&path);
                    self.rescan_directories.insert(path);
                }
            }
            WorkspaceChange::DeletePrefix(path) => {
                self.remove_exact(&path);
                if !self.covered_by_ancestor(&path) {
                    self.remove_descendants(&path);
                    self.deleted_prefixes.insert(path);
                }
            }
        }
        if self.len() >= self.max_changed_paths {
            self.enforce_path_budget();
        }
    }

    pub fn require_full_rescan(&mut self) {
        self.full_rescan = true;
    }

    pub fn is_empty(&self) -> bool {
        !self.full_rescan && self.len() == 0
    }

    pub fn take_batch(&mut self) -> WorkspaceChangeBatch {
        self.collapse();
        let batch = if self.full_rescan {
            WorkspaceChangeBatch {
                changes: vec![WorkspaceChange::Rescan],
            }
        } else {
            let mut changes = Vec::with_capacity(self.len());
            changes.extend(
                self.deleted_prefixes
                    .iter()
                    .cloned()
                    .map(WorkspaceChange::DeletePrefix),
            );
            changes.extend(
                self.rescan_directories
                    .iter()
                    .cloned()
                    .map(WorkspaceChange::RescanDirectory),
            );
            changes.extend(self.deletes.iter().cloned().map(WorkspaceChange::Delete));
            changes.extend(self.upserts.iter().cloned().map(WorkspaceChange::Upsert));
            WorkspaceChangeBatch { changes }
        };
        *self = Self::new(self.max_changed_paths);
        batch
    }

    fn len(&self) -> usize {
        self.upserts.len()
            + self.deletes.len()
            + self.rescan_directories.len()
            + self.deleted_prefixes.len()
    }

    fn covered_by_ancestor(&self, path: &Path) -> bool {
        has_ancestor(&self.rescan_directories, path) || has_ancestor(&self.deleted_prefixes, path)
    }

    fn remove_exact(&mut self, path: &Path) {
        self.upserts.remove(path);
        self.deletes.remove(path);
        self.rescan_directories.remove(path);
        self.deleted_prefixes.remove(path);
    }

    fn remove_descendants(&mut self, path: &Path) {
        self.upserts
            .retain(|candidate| !candidate.starts_with(path));
        self.deletes
            .retain(|candidate| !candidate.starts_with(path));
        self.rescan_directories
            .retain(|candidate| !candidate.starts_with(path));
        self.deleted_prefixes
            .retain(|candidate| !candidate.starts_with(path));
    }

    fn collapse(&mut self) {
        collapse_set(&mut self.rescan_directories);
        collapse_set(&mut self.deleted_prefixes);
        self.upserts.retain(|path| {
            !has_ancestor(&self.rescan_directories, path)
                && !has_ancestor(&self.deleted_prefixes, path)
        });
        self.deletes.retain(|path| {
            !has_ancestor(&self.rescan_directories, path)
                && !has_ancestor(&self.deleted_prefixes, path)
        });
        self.rescan_directories
            .retain(|path| !has_ancestor(&self.deleted_prefixes, path));
    }

    fn enforce_path_budget(&mut self) {
        self.collapse();
        if self.len() < self.max_changed_paths {
            return;
        }
        let leaf_scopes: Vec<_> = self
            .upserts
            .iter()
            .chain(&self.deletes)
            .chain(&self.deleted_prefixes)
            .cloned()
            .collect();
        if !leaf_scopes.is_empty() {
            self.upserts.clear();
            self.deletes.clear();
            self.deleted_prefixes.clear();
            self.rescan_directories
                .extend(leaf_scopes.iter().map(|path| parent_scope(path)));
            self.collapse();
        }
        while self.len() >= self.max_changed_paths {
            let previous_size = self.len();
            self.rescan_directories = self
                .rescan_directories
                .iter()
                .map(|path| parent_scope(path))
                .collect();
            self.collapse();
            if self.len() >= previous_size {
                break;
            }
        }
    }
}

fn collapse_set(paths: &mut BTreeSet<PathBuf>) {
    let sorted: Vec<_> = paths.iter().cloned().collect();
    for path in sorted {
        if has_ancestor(paths, &path) {
            paths.remove(&path);
        }
    }
}

fn has_ancestor(paths: &BTreeSet<PathBuf>, target: &Path) -> bool {
    target
        .ancestors()
        .skip(1)
        .any(|ancestor| paths.contains(ancestor))
}

fn parent_scope(path: &Path) -> PathBuf {
    path.parent().unwrap_or_else(|| Path::new("")).to_path_buf()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::api::WorkspaceChange;

    use super::ChangeSet;

    #[test]
    fn compacts_event_storms_to_parent_directory_scans() {
        let mut changes = ChangeSet::new(3);
        for index in 0..4 {
            changes.add(WorkspaceChange::Upsert(PathBuf::from(format!(
                "src/{index}.rs"
            ))));
        }
        assert_eq!(
            changes.take_batch().changes,
            [WorkspaceChange::RescanDirectory(PathBuf::from("src"))]
        );
    }

    #[test]
    fn deleted_prefix_covers_descendant_changes() {
        let mut changes = ChangeSet::new(1_000);
        changes.add(WorkspaceChange::Upsert(PathBuf::from("target/a.rs")));
        changes.add(WorkspaceChange::DeletePrefix(PathBuf::from("target")));
        changes.add(WorkspaceChange::Upsert(PathBuf::from("target/b.rs")));
        assert_eq!(
            changes.take_batch().changes,
            [WorkspaceChange::DeletePrefix(PathBuf::from("target"))]
        );
    }

    #[test]
    fn last_event_wins_for_the_same_file_path() {
        let mut changes = ChangeSet::new(1_000);
        changes.add(WorkspaceChange::Delete(PathBuf::from("src/lib.rs")));
        changes.add(WorkspaceChange::Upsert(PathBuf::from("src/lib.rs")));
        changes.add(WorkspaceChange::Upsert(PathBuf::from("src/main.rs")));
        changes.add(WorkspaceChange::Delete(PathBuf::from("src/main.rs")));

        assert_eq!(
            changes.take_batch().changes,
            [
                WorkspaceChange::Delete(PathBuf::from("src/main.rs")),
                WorkspaceChange::Upsert(PathBuf::from("src/lib.rs")),
            ]
        );
    }

    #[test]
    fn last_event_wins_for_the_same_directory_path() {
        let mut changes = ChangeSet::new(1_000);
        changes.add(WorkspaceChange::DeletePrefix(PathBuf::from("created")));
        changes.add(WorkspaceChange::RescanDirectory(PathBuf::from("created")));
        changes.add(WorkspaceChange::RescanDirectory(PathBuf::from("removed")));
        changes.add(WorkspaceChange::DeletePrefix(PathBuf::from("removed")));

        assert_eq!(
            changes.take_batch().changes,
            [
                WorkspaceChange::DeletePrefix(PathBuf::from("removed")),
                WorkspaceChange::RescanDirectory(PathBuf::from("created")),
            ]
        );
    }

    #[test]
    fn full_rescan_supersedes_scoped_changes() {
        let mut changes = ChangeSet::new(1_000);
        changes.add(WorkspaceChange::Upsert(PathBuf::from("src/lib.rs")));
        changes.require_full_rescan();
        assert_eq!(changes.take_batch().changes, [WorkspaceChange::Rescan]);
    }
}
