//! Symbol-based incoming and outgoing call queries.

use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// Maximum number of exact definitions in a relationship query.
pub const MAX_SYMBOL_MATCHES: usize = 100;
/// Maximum length of a relationship symbol query in characters.
pub const MAX_RELATIONSHIP_SYMBOL_CHARS: usize = 1024;

/// Direction of a symbol's persisted call relationships.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelationshipDirection {
    Callers,
    Callees,
}

/// Reads an existing workspace index without loading models or refreshing it.
/// All matched definitions and their relationships are read from one consistent store snapshot.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelationshipOptions {
    /// Workspace location. `None` uses the working directory.
    pub root: Option<PathBuf>,
    /// Symbol name, optionally qualified as `scope::name` or `scope.name` (as shown by rg).
    /// `::name` selects only top-level definitions; plain `name` matches any scope.
    /// Absolute qualified paths such as `::ns::name` are equivalent to `ns::name`.
    /// Missing or unrepresentable exact definitions may return similarly named suggestions without edges.
    /// Exact definitions without text locations fail if no usable matches or suggestions remain.
    /// Queries exceeding [`MAX_SYMBOL_MATCHES`] indexed definitions fail; narrow the scope or use `path`.
    pub symbol: String,
    /// Restrict definitions and suggestions to this workspace-relative file or directory.
    /// Directory matches respect path components; this does not filter call-site files.
    pub path: Option<PathBuf>,
    /// Maximum returned call sites per matched symbol; defaults to 20.
    pub limit: Option<usize>,
    /// Maximum workspace lock wait in milliseconds; defaults to 30 seconds.
    pub lock_timeout_ms: Option<u64>,
    /// Runtime-only cooperative cancellation.
    #[serde(skip)]
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

impl RelationshipOptions {
    /// Validate query parameters before opening storage or contacting a daemon.
    ///
    /// # Errors
    /// Returns an invalid-argument error for an empty/overlong symbol, invalid relative path, or zero limit.
    pub fn validate(&self) -> crate::EngineResult<()> {
        let symbol = self.symbol.trim();
        if symbol.is_empty() || symbol.chars().count() > MAX_RELATIONSHIP_SYMBOL_CHARS {
            return Err(crate::EngineError::invalid_argument(format!(
                "symbol must contain between 1 and {MAX_RELATIONSHIP_SYMBOL_CHARS} characters",
            )));
        }
        if symbol.ends_with("::") || symbol.ends_with('.') {
            return Err(crate::EngineError::invalid_argument(
                "symbol name must not be empty",
            ));
        }
        if let Some(path) = &self.path {
            let mut components = path.components();
            if !components
                .clone()
                .any(|part| matches!(part, Component::Normal(_)))
                || !components.all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
            {
                return Err(crate::EngineError::invalid_argument(
                    "relationship path must be a nonempty workspace-relative file or directory without '..'",
                ));
            }
        }
        if self.limit == Some(0) {
            return Err(crate::EngineError::invalid_argument(
                "relationship limit must be positive",
            ));
        }
        Ok(())
    }
}

/// Convert an absolute CLI path to a filter relative to the nearest workspace at `root`.
/// Existing ancestors are canonicalized, so symlinks and missing indexed files work.
/// Selecting the workspace root itself returns `None` (no path restriction).
///
/// # Errors
/// Rejects relative input paths, missing workspaces, filesystem errors and paths outside the workspace.
pub fn workspace_relative_path(root: &Path, path: &Path) -> crate::EngineResult<Option<PathBuf>> {
    if !path.is_absolute() {
        return Err(crate::EngineError::invalid_argument(
            "relationship path must be absolute before conversion",
        ));
    }
    let location = crate::workspace::layout::find_nearest_workspace(root)?.ok_or_else(|| {
        crate::EngineError::not_found("workspace index is unavailable; run `zg --index` first")
    })?;
    let mut ancestor = path;
    let mut resolved = loop {
        match fs::canonicalize(ancestor) {
            Ok(resolved) => break resolved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor.parent().ok_or_else(|| {
                    crate::EngineError::from_io("resolve relationship path", &error)
                })?;
            }
            Err(error) => {
                return Err(crate::EngineError::from_io(
                    "resolve relationship path",
                    &error,
                ));
            }
        }
    };
    for component in path.components().skip(ancestor.components().count()) {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            component => resolved.push(component.as_os_str()),
        }
    }
    let relative = resolved.strip_prefix(&location.root).map_err(|_| {
        crate::EngineError::invalid_argument(format!(
            "relationship path {} is outside workspace {}",
            path.display(),
            location.root.display()
        ))
    })?;
    Ok((!relative.as_os_str().is_empty()).then(|| relative.to_path_buf()))
}

/// A definition in workspace-relative source coordinates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipSymbol {
    pub name: String,
    pub file_path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
}

/// One call site. Unavailable endpoint metadata is represented by `None`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipEdge {
    /// Workspace-relative caller file containing this call site, even without a symbol.
    pub file_path: PathBuf,
    pub symbol: Option<RelationshipSymbol>,
    /// One-based call-site line.
    pub line: Option<usize>,
    /// Zero-based byte column.
    pub column: Option<usize>,
}

/// One matched definition and its call sites, preserving repeated calls.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolRelationships {
    #[serde(flatten)]
    pub symbol: RelationshipSymbol,
    /// Whether additional visible call sites were omitted by the per-symbol limit.
    pub truncated: bool,
    pub edges: Vec<RelationshipEdge>,
}

/// Exact relationship matches, or suggestions to retry when no definition matched.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipResult {
    pub matches: Vec<SymbolRelationships>,
    /// No usable exact definition matched; these similarly named definitions carry no edges.
    /// At most five best-effort suggestions, not an exhaustive list of similar symbols.
    pub suggestions: Vec<RelationshipSymbol>,
}
