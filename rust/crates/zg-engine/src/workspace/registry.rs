//! Optional per-user discovery cache. Workspace manifests are authoritative.

use super::lock::acquire_exclusive_lock;
use crate::{EngineError, EngineResult, domain::Workspace, utils::atomic_write};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

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
    index_dir: PathBuf,
}

impl WorkspaceRegistry {
    pub(crate) fn available_global() -> EngineResult<Option<Self>> {
        if let Some(path) = std::env::var_os("ZVEC_GREP_WORKSPACE_REGISTRY") {
            return Self::at(path.into()).map(Some);
        }
        let Some(config) = crate::config::optional_global_config_path() else {
            return Ok(None);
        };
        let path = config.with_file_name("workspaces.json");
        if !path.parent().is_some_and(Path::is_dir) {
            return Ok(None);
        }
        Self::at(path).map(Some)
    }

    pub(crate) fn at(path: PathBuf) -> EngineResult<Self> {
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(EngineError::invalid_argument(
                "Workspace registry path must be an absolute file path; check ZVEC_GREP_WORKSPACE_REGISTRY",
            ));
        }
        Ok(Self { path })
    }

    pub(crate) fn remember(&self, name: &str, root: &Path, index_dir: &Path) -> EngineResult<()> {
        Workspace::validate_name(name)?;
        let root = canonical_root(root)?;
        let _lock = acquire_exclusive_lock(&self.lock_path(), "remember workspace")?;
        let mut document = self.read()?;
        let index_dir = super::layout::canonical_path(index_dir)?;
        document
            .workspaces
            .retain(|entry| entry.index_dir != index_dir && entry.index_dir.is_dir());
        document.workspaces.push(Registration {
            name: name.to_owned(),
            root,
            index_dir,
        });
        self.write(&document)
    }

    pub(crate) fn forget(&self, root: &Path, index_dir: &Path) -> EngineResult<bool> {
        if !self.path.exists() {
            return Ok(false);
        }
        let root = existing_or_absolute_root(root)?;
        let _lock = acquire_exclusive_lock(&self.lock_path(), "forget workspace")?;
        let mut document = self.read()?;
        let index_dir = super::layout::canonical_path(index_dir)?;
        let before = document.workspaces.len();
        document.workspaces.retain(|entry| {
            (entry.root != root || entry.index_dir != index_dir) && entry.index_dir.is_dir()
        });
        if before == document.workspaces.len() {
            return Ok(false);
        }
        self.write(&document)?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn name_for_root(&self, root: &Path) -> EngineResult<Option<String>> {
        let root = existing_or_absolute_root(root)?;
        Ok(self
            .read()?
            .workspaces
            .into_iter()
            .find(|entry| entry.root == root)
            .map(|entry| entry.name))
    }

    #[cfg(test)]
    pub(crate) fn root_for_name(&self, name: &str) -> EngineResult<Option<PathBuf>> {
        Ok(self
            .read()?
            .workspaces
            .into_iter()
            .find(|entry| entry.name == name)
            .map(|entry| entry.root))
    }

    #[cfg(test)]
    pub(crate) fn rename(&self, _old: &str, new: &str, root: &Path) -> EngineResult<()> {
        self.remember(new, root, &root.join(".zvec-grep"))
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("lock")
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
        let mut roots = HashSet::new();
        for entry in &document.workspaces {
            Workspace::validate_name(&entry.name)
                .map_err(|error| self.invalid(format!("invalid name: {error}")))?;
            if !entry.root.is_absolute() || !entry.index_dir.is_absolute() {
                return Err(self.invalid("workspace roots must be absolute"));
            }
            if !roots.insert((&entry.root, &entry.index_dir)) {
                return Err(self.invalid("workspace locations must be unique"));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrations_keep_one_binding_per_home_and_prune_vanished_homes() {
        let directory = tempfile::tempdir().expect("directory");
        let root_a = directory.path().join("source-a");
        let root_b = directory.path().join("source-b");
        let first = directory.path().join("index-a");
        let second = directory.path().join("index-b");
        for path in [&root_a, &root_b, &first, &second] {
            fs::create_dir(path).expect("directory");
        }
        let registry =
            WorkspaceRegistry::at(directory.path().join("registry.json")).expect("registry");
        registry
            .remember("a", &root_a, &first)
            .expect("first binding");
        registry
            .remember("a", &root_a, &second)
            .expect("second index for same root");
        assert_eq!(registry.read().expect("read").workspaces.len(), 2);
        registry
            .remember("b", &root_b, &first)
            .expect("relocated source");
        let entries = registry.read().expect("read").workspaces;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries.iter().filter(|entry| entry.name == "a").count(), 1);
        assert_eq!(entries.iter().filter(|entry| entry.name == "b").count(), 1);
        fs::remove_dir(second).expect("remove vanished home");
        registry.remember("b", &root_b, &first).expect("prune");
        let entries = registry.read().expect("read").workspaces;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "b");
    }
}
