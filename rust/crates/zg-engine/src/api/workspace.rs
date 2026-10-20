//! Source and data locations resolved using the engine's workspace rules.

use std::path::{Path, PathBuf};

use crate::{EngineResult, workspace::layout};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WorkspaceLocation {
    pub root: PathBuf,
    pub home: PathBuf,
}

impl WorkspaceLocation {
    /// Resolves an explicit source, or the source saved in an existing index.
    ///
    /// # Errors
    /// Returns an error when the source relationship cannot be resolved.
    pub fn for_index(root: Option<&Path>, index_dir: Option<&Path>) -> EngineResult<Self> {
        let location = layout::index_location(root, index_dir)?;
        Ok(Self {
            root: location.root,
            home: location.home,
        })
    }

    /// Resolves an exact source root and its data directory for creation.
    ///
    /// # Errors
    /// Returns an error if the locations cannot be resolved or overlap unsafely.
    pub fn resolve(root: Option<&Path>, index_dir: Option<&Path>) -> EngineResult<Self> {
        let root = layout::resolve_workspace_root(root)?;
        let location = layout::workspace_index_location(&root, index_dir)?;
        Ok(Self {
            root: location.root,
            home: location.home,
        })
    }

    /// Finds the default ancestor workspace, or resolves the explicitly selected directory.
    /// If no workspace exists, resolves where its initial index would be created.
    ///
    /// # Errors
    /// Returns an error if the locations cannot be resolved or overlap unsafely.
    pub fn discover(root: Option<&Path>, index_dir: Option<&Path>) -> EngineResult<Self> {
        let root = layout::resolve_workspace_root(root)?;
        let location = match layout::find_nearest_workspace(&root, index_dir)? {
            Some(location) => location,
            None => layout::workspace_index_location(&root, index_dir)?,
        };
        Ok(Self {
            root: location.root,
            home: location.home,
        })
    }
}
