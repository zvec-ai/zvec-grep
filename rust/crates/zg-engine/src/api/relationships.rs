//! Symbol-based incoming and outgoing call queries.

use std::path::{Path, PathBuf};

pub use crate::domain::GlobRule;

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
/// Each matched definition and its call sites are read from one consistent store snapshot.
/// Different matches may reflect different indexing moments; roots changed since selection are skipped.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelationshipOptions {
    /// Workspace location. `None` uses the working directory.
    pub root: Option<PathBuf>,
    /// Symbol name, optionally qualified as `scope::name` or `scope.name` (as shown by rg).
    /// `::name` selects only top-level definitions; plain `name` matches any scope.
    /// Absolute qualified paths such as `::ns::name` are equivalent to `ns::name`.
    /// Missing or unrepresentable exact definitions may return similarly named suggestions without edges.
    /// Queries exceeding [`MAX_SYMBOL_MATCHES`] indexed definitions fail; narrow the scope or use `globs`.
    pub symbol: String,
    /// Ordered workspace-relative glob rules, using the same semantics as indexed search.
    /// Filter definitions and suggestions; call-site files remain unrestricted.
    pub globs: Vec<GlobRule>,
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
    /// Returns an invalid-argument error for an empty/overlong symbol, invalid glob, or zero limit.
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
        crate::file_selection::GlobMatcher::new(Path::new(""), &self.globs)?;
        if self.limit == Some(0) {
            return Err(crate::EngineError::invalid_argument(
                "relationship limit must be positive",
            ));
        }
        Ok(())
    }
}

/// A definition in workspace-relative source coordinates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDefinition {
    pub name: String,
    pub file_path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
}

/// One call site. Unavailable endpoint metadata is represented by `None`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallSite {
    /// Workspace-relative caller file containing this call site, even without a symbol.
    pub file_path: PathBuf,
    pub symbol: Option<SymbolDefinition>,
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
    pub symbol: SymbolDefinition,
    /// Whether additional visible call sites were omitted by the per-symbol limit.
    pub truncated: bool,
    pub edges: Vec<CallSite>,
}

/// Exact relationship matches, or suggestions to retry when no definition matched.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipResult {
    pub matches: Vec<SymbolRelationships>,
    /// No usable exact definition matched; these similarly named definitions carry no edges.
    /// At most five best-effort suggestions, not an exhaustive list of similar symbols.
    pub suggestions: Vec<SymbolDefinition>,
}
