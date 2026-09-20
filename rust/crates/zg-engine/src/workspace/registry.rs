//! Per-user workspace names and their source directories.
//!
//! A registration is also a durable reservation: it is saved before building a
//! first index and survives interrupted builds. Callers acquire the workspace
//! home lock before entering the registry and explicitly unregister on deletion.
//! An externally deleted root can be unregistered using only the registry lock.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{EngineError, EngineResult, domain::Workspace, utils::atomic_write};

use super::lock::acquire_exclusive_lock;

#[derive(Clone, Debug)]
pub(crate) struct WorkspaceRegistry {
    path: PathBuf,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RegistryDocument {
    version: u32,
    workspaces: Vec<Registration>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    name: String,
    root: PathBuf,
}

impl WorkspaceRegistry {
    pub(crate) fn global() -> EngineResult<Self> {
        let path = std::env::var_os("ZVEC_GREP_WORKSPACE_REGISTRY").map_or_else(
            || {
                crate::config::global_config_path()
                    .map(|path| path.with_file_name("workspaces.json"))
            },
            |path| Ok(PathBuf::from(path)),
        )?;
        Self::at(path)
    }

    /// Inject an isolated registry for tests or an explicitly configured engine.
    pub(crate) fn at(path: PathBuf) -> EngineResult<Self> {
        let valid_name = path.file_name().is_some_and(|name| {
            path.as_os_str()
                .as_encoded_bytes()
                .ends_with(name.as_encoded_bytes())
        });
        if !path.is_absolute() || !valid_name {
            return Err(EngineError::invalid_argument(
                "Workspace registry path must be an absolute file path; check ZVEC_GREP_WORKSPACE_REGISTRY",
            ));
        }
        Ok(Self { path })
    }

    /// Reserve a name before creating its first manifest. Retrying is idempotent.
    pub(crate) fn register(&self, name: &str, root: &Path) -> EngineResult<()> {
        Workspace::validate_name(name)?;
        let root = canonical_root(root)?;
        let _lock = acquire_exclusive_lock(&self.lock_path(), "register workspace")?;
        let mut document = self.read()?;
        if let Some(existing) = document.workspaces.iter().find(|entry| entry.name == name) {
            if existing.root == root {
                return Ok(());
            }
            return Err(name_conflict(name, &existing.root));
        }
        ensure_root_available(&document, &root)?;
        document.workspaces.push(Registration {
            name: name.to_owned(),
            root,
        });
        self.write(&document)
    }

    pub(crate) fn name_for_root(&self, root: &Path) -> EngineResult<Option<String>> {
        let root = existing_or_absolute_root(root)?;
        // Atomic replacement lets lookups read a complete snapshot without
        // creating a lock file or a previously absent configuration directory.
        Ok(self
            .read()?
            .workspaces
            .into_iter()
            .find(|entry| entry.root == root)
            .map(|entry| entry.name))
    }

    pub(crate) fn root_for_name(&self, name: &str) -> EngineResult<Option<PathBuf>> {
        Workspace::validate_name(name)?;
        Ok(self
            .read()?
            .workspaces
            .into_iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.root))
    }

    /// Commit the authoritative name before updating the local manifest.
    /// A retry also succeeds after another workspace has reused the old name.
    pub(crate) fn rename(&self, old_name: &str, new_name: &str, root: &Path) -> EngineResult<()> {
        Workspace::validate_name(old_name)?;
        Workspace::validate_name(new_name)?;
        let root = canonical_root(root)?;
        let _lock = acquire_exclusive_lock(&self.lock_path(), "rename workspace")?;
        let mut document = self.read()?;
        if let Some(existing) = document
            .workspaces
            .iter()
            .find(|entry| entry.name == new_name)
        {
            return if existing.root == root {
                Ok(())
            } else {
                Err(name_conflict(new_name, &existing.root))
            };
        }
        let existing = document
            .workspaces
            .iter_mut()
            .find(|entry| entry.name == old_name)
            .ok_or_else(|| {
                EngineError::not_found(format!("Workspace name '{old_name}' is not registered"))
            })?;
        if existing.root != root {
            return Err(name_conflict(old_name, &existing.root));
        }
        new_name.clone_into(&mut existing.name);
        self.write(&document)
    }

    /// Remove only the expected name/root pair, including an abandoned reservation.
    pub(crate) fn unregister(&self, name: &str, root: &Path) -> EngineResult<()> {
        Workspace::validate_name(name)?;
        let root = existing_or_absolute_root(root)?;
        let _lock = acquire_exclusive_lock(&self.lock_path(), "unregister workspace")?;
        let mut document = self.read()?;
        let Some(index) = document
            .workspaces
            .iter()
            .position(|entry| entry.name == name)
        else {
            return Ok(());
        };
        if document.workspaces[index].root != root {
            return Err(name_conflict(name, &document.workspaces[index].root));
        }
        document.workspaces.remove(index);
        self.write(&document)
    }

    /// Release a deleted source directory without recreating it to obtain a home lock.
    pub(crate) fn unregister_missing(&self, root: &Path) -> EngineResult<bool> {
        let _lock = acquire_exclusive_lock(&self.lock_path(), "unregister deleted workspace")?;
        let root = existing_or_absolute_root(root)?;
        match fs::symlink_metadata(&root) {
            Ok(_) => {
                return Err(EngineError::resource_busy(format!(
                    "Workspace root {} exists; retry dropping it with the workspace home lock",
                    root.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(EngineError::from_io(
                    format!("check deleted workspace root {}", root.display()),
                    &error,
                ));
            }
        }
        let mut document = self.read()?;
        let Some(index) = document
            .workspaces
            .iter()
            .position(|entry| entry.root == root)
        else {
            return Ok(false);
        };
        document.workspaces.remove(index);
        self.write(&document)?;
        Ok(true)
    }

    /// Adopt an explicitly moved workspace without allowing a copy to steal its name.
    pub(crate) fn relocate(
        &self,
        name: &str,
        old_root: &Path,
        new_root: &Path,
    ) -> EngineResult<()> {
        Workspace::validate_name(name)?;
        let old_root = existing_or_absolute_root(old_root)?;
        let new_root = canonical_root(new_root)?;
        let _lock = acquire_exclusive_lock(&self.lock_path(), "relocate workspace")?;
        let mut document = self.read()?;
        let index = document
            .workspaces
            .iter()
            .position(|entry| entry.name == name)
            .ok_or_else(|| {
                EngineError::not_found(format!("Workspace name '{name}' is not registered"))
            })?;
        let registered_root = &document.workspaces[index].root;
        if *registered_root == new_root {
            return Ok(());
        }
        if *registered_root != old_root {
            return Err(name_conflict(name, registered_root));
        }
        if old_root.try_exists().map_err(|error| {
            EngineError::from_io(
                format!("inspect original workspace {}", old_root.display()),
                &error,
            )
        })? {
            return Err(name_conflict(name, &old_root).with_help("The original workspace directory still exists. Move it instead of copying it, or choose a different name for the copy."));
        }
        ensure_root_available(&document, &new_root)?;
        document.workspaces[index].root = new_root;
        self.write(&document)
    }

    fn lock_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_os_string();
        name.push(".lock");
        PathBuf::from(name)
    }

    fn read(&self) -> EngineResult<RegistryDocument> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RegistryDocument {
                    version: 1,
                    workspaces: Vec::new(),
                });
            }
            Err(error) => return Err(self.io_error("read", &error)),
        };
        let document: RegistryDocument = serde_json::from_slice(&bytes)
            .map_err(|error| self.invalid(format!("invalid JSON: {error}")))?;
        if document.version != 1 {
            return Err(self.invalid(format!("unsupported version {}", document.version)));
        }
        let mut names = HashSet::new();
        let mut roots = HashSet::new();
        for entry in &document.workspaces {
            Workspace::validate_name(&entry.name)
                .map_err(|error| self.invalid(format!("invalid name: {error}")))?;
            if !entry.root.is_absolute() {
                return Err(self.invalid("workspace roots must be absolute"));
            }
            if !names.insert(&entry.name) || !roots.insert(&entry.root) {
                return Err(self.invalid("workspace names and roots must both be unique"));
            }
        }
        Ok(document)
    }

    fn write(&self, document: &RegistryDocument) -> EngineResult<()> {
        let mut bytes = serde_json::to_vec_pretty(document).map_err(|error| {
            EngineError::internal(format!("failed to encode workspace registry: {error}"))
        })?;
        bytes.push(b'\n');
        atomic_write(&self.path, &bytes)
    }

    fn invalid(&self, cause: impl std::fmt::Display) -> EngineError {
        EngineError::invalid_argument(format!(
            "Invalid workspace registry {}: {cause}",
            self.path.display()
        ))
    }

    fn io_error(&self, operation: &str, source: &std::io::Error) -> EngineError {
        EngineError::from_io(
            format!("{operation} workspace registry {}", self.path.display()),
            source,
        )
    }
}

fn canonical_root(root: &Path) -> EngineResult<PathBuf> {
    let path = fs::canonicalize(root).map_err(|error| {
        EngineError::from_io(format!("resolve workspace root {}", root.display()), &error)
    })?;
    if !path.is_dir() {
        return Err(EngineError::invalid_argument(format!(
            "Workspace root {} must be a directory",
            root.display()
        )));
    }
    Ok(path)
}

fn existing_or_absolute_root(root: &Path) -> EngineResult<PathBuf> {
    let mut ancestor = root;
    loop {
        match fs::canonicalize(ancestor) {
            Ok(path) => {
                // The old root can disappear during a move. Resolve the existing
                // prefix so aliases such as /tmp -> /private/tmp still compare
                // with the canonical path saved at registration time.
                let suffix = root.strip_prefix(ancestor).map_err(|error| {
                    EngineError::internal(format!("resolve registered workspace root: {error}"))
                })?;
                return Ok(path.join(suffix));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && root.is_absolute() => {
                if let Some(parent) = ancestor.parent() {
                    ancestor = parent;
                    continue;
                }
                return Err(EngineError::from_io(
                    format!("resolve registered workspace root {}", root.display()),
                    &error,
                ));
            }
            Err(error) => {
                return Err(EngineError::from_io(
                    format!("resolve registered workspace root {}", root.display()),
                    &error,
                ));
            }
        }
    }
}

fn ensure_root_available(document: &RegistryDocument, root: &Path) -> EngineResult<()> {
    if let Some(existing) = document.workspaces.iter().find(|entry| entry.root == root) {
        return Err(EngineError::invalid_argument(format!(
            "Workspace root {} is already registered as '{}'",
            root.display(),
            existing.name,
        ))
        .with_help(
            "Use its registered name; changing a workspace name requires an explicit rename.",
        ));
    }
    Ok(())
}

fn name_conflict(name: &str, root: &Path) -> EngineError {
    EngineError::invalid_argument(format!("Workspace name '{name}' is already registered for {}", root.display()))
        .with_help("Choose a different workspace name, or explicitly relocate or delete the existing workspace.")
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier, mpsc},
        time::Duration,
    };

    use tempfile::tempdir;

    use super::*;

    fn name(value: &str) -> String {
        value.to_owned()
    }

    #[test]
    fn invalid_string_names_never_mutate_the_registry() {
        let directory = tempdir().expect("workspace");
        let path = directory.path().join("workspaces.json");
        let registry = WorkspaceRegistry::at(path.clone()).expect("registry");
        let root = directory.path();
        assert!(registry.register("bad/name", root).is_err());
        assert!(!path.exists());
        assert!(!registry.lock_path().exists());
        registry.register("project", root).expect("register");
        let original = fs::read(&path).expect("original registry");
        for invalid in ["", " ", ".", "..", "a/b", "a\\b", "a\nb"] {
            assert!(registry.register(invalid, root).is_err());
            assert!(registry.rename("project", invalid, root).is_err());
            assert!(registry.rename(invalid, "project", root).is_err());
            assert!(registry.unregister(invalid, root).is_err());
            assert!(registry.relocate(invalid, root, root).is_err());
            assert!(registry.root_for_name(invalid).is_err());
            assert_eq!(fs::read(&path).expect("preserved registry"), original);
        }
    }

    #[test]
    fn names_remain_case_sensitive_registry_keys() {
        let directory = tempdir().expect("workspace roots");
        let registry =
            WorkspaceRegistry::at(directory.path().join("workspaces.json")).expect("registry");
        for (index, name) in ["Backend", "backend", "项目 backend"]
            .into_iter()
            .enumerate()
        {
            // Use separate numbered roots even on case-insensitive filesystems.
            let root = directory.path().join(index.to_string());
            fs::create_dir(&root).expect("workspace root");
            registry.register(name, &root).expect("distinct name");
            assert_eq!(
                registry.name_for_root(&root).expect("stored name"),
                Some(name.to_owned())
            );
            assert_eq!(
                registry.root_for_name(name).expect("stored root"),
                Some(fs::canonicalize(&root).expect("root"))
            );
        }
        assert_eq!(registry.read().expect("registry").workspaces.len(), 3);
    }

    #[test]
    fn reservation_survives_reopening_and_conflicts_preserve_the_registry() {
        let directory = tempdir().expect("temporary directory");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir(&first).expect("first root");
        fs::create_dir(&second).expect("second root");
        let path = directory.path().join("config/workspaces.json");
        let registry = WorkspaceRegistry::at(path.clone()).expect("registry");
        registry
            .register(&name("project"), &first)
            .expect("reserve before first build");
        let bytes = fs::read(&path).expect("durable reservation");
        drop(registry);

        let registry = WorkspaceRegistry::at(path.clone()).expect("reopened registry");
        assert_eq!(
            registry.name_for_root(&first).expect("recover name"),
            Some(name("project"))
        );
        registry
            .register(&name("project"), &first.join("."))
            .expect("idempotent retry");
        let error = registry
            .register(&name("project"), &second)
            .expect_err("name collision");
        assert!(error.to_string().contains("already registered"));
        assert!(registry.register(&name("different"), &first).is_err());
        assert_eq!(fs::read(&path).expect("unchanged registry"), bytes);
    }

    #[test]
    fn unregister_requires_the_matching_root_and_allows_name_reuse() {
        let directory = tempdir().expect("temporary directory");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir(&first).expect("first root");
        fs::create_dir(&second).expect("second root");
        let registry =
            WorkspaceRegistry::at(directory.path().join("workspaces.json")).expect("registry");
        registry
            .register(&name("project"), &first)
            .expect("reserve");
        assert!(registry.unregister(&name("project"), &second).is_err());
        assert_eq!(
            registry.name_for_root(&first).expect("retained"),
            Some(name("project"))
        );
        registry
            .unregister(&name("project"), &first)
            .expect("remove matching pair");
        registry
            .unregister(&name("project"), &first)
            .expect("repeat deletion");
        registry
            .register(&name("project"), &second)
            .expect("reuse released name");
    }

    #[test]
    fn unregistering_a_deleted_root_releases_its_name_without_recreating_directories() {
        let directory = tempdir().expect("temporary directory");
        let deleted = directory.path().join("deleted");
        let replacement = directory.path().join("replacement");
        fs::create_dir(&deleted).expect("original root");
        fs::create_dir(&replacement).expect("replacement root");
        let registry =
            WorkspaceRegistry::at(directory.path().join("workspaces.json")).expect("registry");
        registry
            .register(&name("project"), &deleted)
            .expect("register original");
        fs::remove_dir(&deleted).expect("externally delete source directory");

        assert!(
            registry
                .unregister_missing(&deleted)
                .expect("release missing root")
        );
        assert!(!deleted.exists());
        assert!(
            !registry
                .unregister_missing(&deleted)
                .expect("idempotent cleanup")
        );
        registry
            .register(&name("project"), &replacement)
            .expect("reuse released name");
        assert_eq!(
            registry.root_for_name(&name("project")).expect("new owner"),
            Some(fs::canonicalize(&replacement).expect("replacement root"))
        );
    }

    #[test]
    fn unregister_missing_rechecks_live_roots_and_preserves_their_registration() {
        let directory = tempdir().expect("temporary directory");
        let root = directory.path().join("root");
        fs::create_dir(&root).expect("source directory");
        let path = directory.path().join("workspaces.json");
        let registry = WorkspaceRegistry::at(path.clone()).expect("registry");
        registry
            .register(&name("project"), &root)
            .expect("registered root");
        let before = fs::read(&path).expect("original registry");
        let error = registry
            .unregister_missing(&root)
            .expect_err("live root needs home lock");
        assert_eq!(error.code(), EngineError::RESOURCE_BUSY);
        assert_eq!(fs::read(path).expect("registry unchanged"), before);
        assert!(!root.join(".zvec-grep").exists());
    }

    #[test]
    fn rename_rejects_collisions_and_wrong_roots_without_changing_registration() {
        let directory = tempdir().expect("temporary directory");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir(&first).expect("first root");
        fs::create_dir(&second).expect("second root");
        let path = directory.path().join("workspaces.json");
        let registry = WorkspaceRegistry::at(path.clone()).expect("registry");
        registry
            .register(&name("first"), &first)
            .expect("register first");
        registry
            .register(&name("second"), &second)
            .expect("register second");
        let bytes = fs::read(&path).expect("original registry");

        assert!(
            registry
                .rename(&name("first"), &name("second"), &first)
                .is_err()
        );
        assert!(
            registry
                .rename(&name("first"), &name("new"), &second)
                .is_err()
        );
        assert!(
            registry
                .rename(&name("missing"), &name("new"), &first)
                .is_err()
        );
        assert_eq!(fs::read(&path).expect("unchanged registry"), bytes);
    }

    #[test]
    fn rename_replay_succeeds_after_the_old_name_is_reused() {
        let directory = tempdir().expect("temporary directory");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir(&first).expect("first root");
        fs::create_dir(&second).expect("second root");
        let registry =
            WorkspaceRegistry::at(directory.path().join("workspaces.json")).expect("registry");
        registry
            .register(&name("old"), &first)
            .expect("register first");
        registry
            .rename(&name("old"), &name("new"), &first)
            .expect("rename");
        assert_eq!(
            registry
                .name_for_root(&first)
                .expect("recover authoritative name"),
            Some(name("new"))
        );
        assert_eq!(
            registry
                .root_for_name(&name("old"))
                .expect("released old name"),
            None
        );
        registry
            .register(&name("old"), &second)
            .expect("reuse old name");

        registry
            .rename(&name("old"), &name("new"), &first.join("."))
            .expect("retry after crash");
        registry
            .rename(&name("new"), &name("new"), &first)
            .expect("same name");
        assert_eq!(
            registry
                .root_for_name(&name("new"))
                .expect("first workspace"),
            Some(fs::canonicalize(&first).expect("canonical first root"))
        );
        assert_eq!(
            registry
                .root_for_name(&name("old"))
                .expect("second workspace"),
            Some(fs::canonicalize(&second).expect("canonical second root"))
        );
    }

    #[test]
    fn relocation_accepts_a_move_and_rejects_a_live_copy() {
        let directory = tempdir().expect("temporary directory");
        let original = directory.path().join("original");
        let copy = directory.path().join("copy");
        let moved = directory.path().join("moved");
        fs::create_dir_all(original.join(".zvec-grep")).expect("workspace home");
        fs::write(original.join(".zvec-grep/manifest.json"), b"{}").expect("workspace manifest");
        fs::create_dir(&copy).expect("copy root");
        let registry =
            WorkspaceRegistry::at(directory.path().join("workspaces.json")).expect("registry");
        registry
            .register(&name("project"), &original)
            .expect("register original");
        assert!(
            registry
                .relocate(&name("project"), &original, &copy)
                .is_err()
        );
        fs::rename(&original, &moved).expect("move original");
        registry
            .relocate(&name("project"), &original, &moved)
            .expect("register move");
        registry
            .relocate(&name("project"), &original, &moved)
            .expect("idempotent retry");
        assert_eq!(
            registry.name_for_root(&moved).expect("new location"),
            Some(name("project"))
        );
        assert!(registry.unregister(&name("project"), &original).is_err());
    }

    #[test]
    fn malformed_registries_are_rejected_without_overwriting_them() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("workspaces.json");
        let registry = WorkspaceRegistry::at(path.clone()).expect("registry");
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        for value in [
            serde_json::json!({"version": 2, "workspaces": []}),
            serde_json::json!({"version": 1, "workspaces": [{"name": "bad/name", "root": root}]}),
            serde_json::json!({"version": 1, "workspaces": [{"name": "project", "root": "relative"}]}),
            serde_json::json!({"version": 1, "workspaces": [{"name": "project", "root": root}, {"name": "project", "root": root.join("other")}]}),
            serde_json::json!({"version": 1, "workspaces": [{"name": "project", "root": root}, {"name": "other", "root": root}]}),
        ] {
            let bytes = serde_json::to_vec(&value).expect("test JSON");
            fs::write(&path, &bytes).expect("existing registry");
            assert!(registry.register(&name("new"), directory.path()).is_err());
            assert_eq!(fs::read(&path).expect("preserved registry"), bytes);
        }
    }

    #[test]
    fn concurrent_reservations_cannot_claim_the_same_name() {
        let directory = tempdir().expect("temporary directory");
        let registry =
            WorkspaceRegistry::at(directory.path().join("workspaces.json")).expect("registry");
        let barrier = Arc::new(Barrier::new(2));
        let mut threads = Vec::new();
        for index in 0..2 {
            let root = directory.path().join(index.to_string());
            fs::create_dir(&root).expect("workspace root");
            let registry = registry.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                registry.register(&name("project"), &root)
            }));
        }
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().expect("registration thread"))
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let error = results
            .iter()
            .find_map(|result| result.as_ref().err())
            .expect("name conflict");
        assert_eq!(error.code(), EngineError::INVALID_ARGUMENT);
        assert!(error.message().contains("already registered"));
        assert_eq!(
            registry
                .read()
                .expect("registry after race")
                .workspaces
                .len(),
            1
        );
    }

    #[test]
    fn concurrent_distinct_names_are_both_registered() {
        let directory = tempdir().expect("temporary directory");
        let registry =
            WorkspaceRegistry::at(directory.path().join("workspaces.json")).expect("registry");
        let barrier = Arc::new(Barrier::new(2));
        let mut threads = Vec::new();
        for index in 0..2 {
            let root = directory.path().join(index.to_string());
            fs::create_dir(&root).expect("workspace root");
            let registry = registry.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                registry.register(&name(&format!("project-{index}")), &root)
            }));
        }
        for thread in threads {
            thread
                .join()
                .expect("registration thread")
                .expect("independent workspace registration");
        }
        for index in 0..2 {
            assert_eq!(
                registry
                    .root_for_name(&name(&format!("project-{index}")))
                    .expect("registered name"),
                Some(
                    fs::canonicalize(directory.path().join(index.to_string()))
                        .expect("canonical root")
                )
            );
        }
    }

    #[test]
    fn registry_writer_waits_for_the_previous_writer_to_release_its_lock() {
        let directory = tempdir().expect("temporary directory");
        let registry =
            WorkspaceRegistry::at(directory.path().join("workspaces.json")).expect("registry");
        let lock = acquire_exclusive_lock(&registry.lock_path(), "test").expect("registry lock");
        let (started_tx, started_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        let waiting_registry = registry.clone();
        let root = directory.path().to_path_buf();
        let thread = std::thread::spawn(move || {
            started_tx.send(()).expect("writer started");
            completed_tx
                .send(waiting_registry.register(&name("project"), &root))
                .expect("writer result");
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer thread starts");
        let while_locked = completed_rx.recv_timeout(Duration::from_millis(50));
        drop(lock);
        assert!(matches!(while_locked, Err(mpsc::RecvTimeoutError::Timeout)));
        completed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer wakes after release")
            .expect("registration succeeds");
        thread.join().expect("writer thread");
        assert_eq!(
            registry
                .name_for_root(directory.path())
                .expect("registered root"),
            Some(name("project"))
        );
    }

    #[test]
    fn absent_lookup_does_not_create_a_registry() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("config/workspaces.json");
        let registry = WorkspaceRegistry::at(path.clone()).expect("registry");
        assert_eq!(
            registry.name_for_root(directory.path()).expect("lookup"),
            None
        );
        assert_eq!(
            registry
                .root_for_name(&name("project"))
                .expect("name lookup"),
            None
        );
        assert!(!path.parent().expect("config parent").exists());
        assert!(WorkspaceRegistry::at(PathBuf::from("relative.json")).is_err());
    }

    #[test]
    fn existing_registry_lookups_do_not_create_a_lock_file() {
        let directory = tempdir().expect("temporary directory");
        let root = fs::canonicalize(directory.path()).expect("canonical root");
        let path = directory.path().join("workspaces.json");
        let registry = WorkspaceRegistry::at(path.clone()).expect("registry");
        let document =
            serde_json::json!({"version": 1, "workspaces": [{"name": "project", "root": root}]});
        fs::write(&path, serde_json::to_vec(&document).expect("registry JSON"))
            .expect("existing registry");

        assert_eq!(
            registry
                .name_for_root(directory.path())
                .expect("root lookup"),
            Some(name("project"))
        );
        assert_eq!(
            registry
                .root_for_name(&name("project"))
                .expect("name lookup"),
            Some(root)
        );
        assert!(!registry.lock_path().exists());
    }
}
